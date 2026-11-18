//! 控制面最小集：ClientHello → Auth → ChannelSync → JoinChannel → UserJoined/Left → Ping/Pong。
//!
//! **先不做**：Whisper、多频道监听、管理操作、频道增删（这些消息在 proto 里有号，但这里只忽略并计数）。
//!
//! 这一层同样是 sans-io：吃一条 {@ControlMessage@}，吐一串 {@Action@}（发给谁、发什么、
//! 要不要关连接）。真正的 TCP + TLS 在传输层，测试里就是一个 `Vec`。

use protocol::control::proto as pb;
use protocol::control::proto::control_message::Kind;
use protocol::{ControlMessage, PROTOCOL_VERSION, caps};

use crate::auth::AuthMethod;
use crate::channel::ChannelId;
use crate::session::{SessionId, SessionState};
use crate::{Server, Session};

/// 控制面要执行的动作。
#[derive(Debug)]
pub enum Action {
    /// 给某个会话发一条消息。
    Send {
        /// 目标会话。
        to: SessionId,
        /// 消息。
        msg: ControlMessage,
    },
    /// 关闭某个会话（随后服务器会清理它）。
    Close {
        /// 目标会话。
        session: SessionId,
        /// 错误码（{@pb::ErrorCode@} 的数值；proto 里是 uint32）。
        code: u32,
        /// 人类可读的原因，只进日志。
        message: String,
    },
}

impl Action {
    fn send(to: SessionId, msg: ControlMessage) -> Self {
        Self::Send { to, msg }
    }
}

fn envelope(request_id: u64, kind: Kind) -> ControlMessage {
    ControlMessage {
        protocol: PROTOCOL_VERSION,
        request_id,
        kind: Some(kind),
    }
}

fn error_msg(code: pb::ErrorCode, message: &str, fatal: bool) -> ControlMessage {
    envelope(
        0,
        Kind::Error(pb::Error {
            code: code as u32,
            message: message.to_owned(),
            fatal,
        }),
    )
}

impl Server {
    /// 处理一条控制面消息。
    ///
    /// 返回的 {@Action@} 由传输层按顺序执行；`Close` 之后这个会话不会再产生动作。
    pub fn handle_control(
        &mut self,
        session_id: SessionId,
        msg: ControlMessage,
        now_ms: u64,
    ) -> Vec<Action> {
        self.metrics_mut().control_msgs += 1;

        let Some(session) = self.session(session_id) else {
            return Vec::new();
        };
        if session.state == SessionState::Closed {
            return Vec::new();
        }

        // 限速：超速只回一条 RATE_LIMITED，不断连接（断连接会让一次突发变成掉线）。
        let allowed = self
            .session_mut(session_id)
            .is_some_and(|s| s.control_rate.allow(now_ms));
        if !allowed {
            self.metrics_mut().control_rate_limited += 1;
            return vec![Action::send(
                session_id,
                error_msg(pb::ErrorCode::RateLimited, "控制消息超速", false),
            )];
        }

        let request_id = msg.request_id;
        match msg.kind {
            Some(Kind::ClientHello(hello)) => {
                self.on_client_hello(session_id, request_id, hello, now_ms)
            }
            Some(Kind::Auth(auth)) => self.on_auth(session_id, request_id, auth),
            Some(Kind::JoinChannel(join)) => self.on_join_channel(session_id, join),
            Some(Kind::LeaveChannel(_)) => self.on_leave_channel(session_id),
            Some(Kind::SetSelfState(state)) => self.on_set_self_state(session_id, state),
            Some(Kind::Ping(ping)) => vec![Action::send(
                session_id,
                envelope(request_id, Kind::Pong(pb::Pong { ts_us: ping.ts_us })),
            )],
            Some(Kind::RekeyRequest(_)) => {
                // v1 不做在线换钥：把当前 epoch 回过去，客户端知道「还是这把钥匙」。
                // 真正的轮换走重连（协议草案 §10 第 6 条）。
                let epoch = u32::from(self.config().voice_key_epoch);
                vec![Action::send(
                    session_id,
                    envelope(
                        request_id,
                        Kind::RekeyResponse(pb::RekeyResponse { key_epoch: epoch }),
                    ),
                )]
            }
            _ => {
                // 未知 / 未实现的 oneof 分支：忽略并计数（协议草案 §9）。
                self.metrics_mut().control_ignored += 1;
                Vec::new()
            }
        }
    }

    fn on_client_hello(
        &mut self,
        session_id: SessionId,
        request_id: u64,
        hello: pb::ClientHello,
        now_ms: u64,
    ) -> Vec<Action> {
        let Some(session) = self.session(session_id) else {
            return Vec::new();
        };
        if session.state != SessionState::AwaitHello {
            self.metrics_mut().control_ignored += 1;
            return Vec::new();
        }
        let min = if hello.protocol_min == 0 {
            1
        } else {
            hello.protocol_min
        };
        let max = if hello.protocol_max == 0 {
            1
        } else {
            hello.protocol_max
        };
        if !(min..=max).contains(&PROTOCOL_VERSION) {
            return vec![
                Action::send(
                    session_id,
                    error_msg(pb::ErrorCode::UnsupportedVersion, "协议版本不兼容", true),
                ),
                Action::Close {
                    session: session_id,
                    code: pb::ErrorCode::UnsupportedVersion as u32,
                    message: format!("客户端支持 {min}..={max}，服务器只支持 {PROTOCOL_VERSION}"),
                },
            ];
        }

        // 能力位取交集：双方都置位才启用。
        let server_caps = caps::RED | caps::DTX | caps::STATS;
        let agreed = hello.caps & server_caps;

        let (session_id_u64, sender_id, voice_token, key_epoch, channel_epoch) = {
            let s = self.session(session_id).expect("上面查过");
            (
                s.id,
                s.sender_id,
                s.voice_token,
                u32::from(self.config().voice_key_epoch),
                s.channel
                    .and_then(|c| self.channel(c))
                    .map_or(0, |c| u32::from(c.epoch)),
            )
        };
        if let Some(s) = self.session_mut(session_id) {
            s.state = SessionState::AwaitAuth;
        }
        let limits = self.config().limits;
        let hello_out = pb::ServerHello {
            protocol: PROTOCOL_VERSION,
            server_version: self.config().server_version.clone(),
            session_id: session_id_u64,
            sender_id,
            voice_host: self.config().voice_host.clone(),
            voice_port: u32::from(self.config().voice_port),
            voice_token: voice_token.to_vec(),
            key_epoch,
            channel_epoch,
            server_time_ms: now_ms,
            caps: agreed,
            limits: Some(limits),
        };
        vec![Action::send(
            session_id,
            envelope(request_id, Kind::ServerHello(hello_out)),
        )]
    }

    fn on_auth(&mut self, session_id: SessionId, request_id: u64, auth: pb::Auth) -> Vec<Action> {
        let Some(session) = self.session(session_id) else {
            return Vec::new();
        };
        if session.state != SessionState::AwaitAuth {
            self.metrics_mut().control_ignored += 1;
            return Vec::new();
        }

        // 先把消息本体绑到局部变量上，`method` 借的是它的字段；
        // 直接 match `auth.method` 会让被借的值在 match 臂结束时就被丢掉。
        let method_msg = auth.method;
        let method = match &method_msg {
            Some(pb::auth::Method::Password(p)) => AuthMethod::Password {
                username: &p.username,
                password: &p.password,
            },
            // 用户名为空 = 只要一个「凭设备号进来」的身份，服务器没见过这个设备就建一个
            // 匿名账号（协议草案 §12.13）。这条路上没有账号可以「冒名」，所以不需要用户名。
            Some(pb::auth::Method::DeviceToken(t)) if t.username.is_empty() => {
                AuthMethod::Anonymous {
                    token: &t.device_token,
                }
            }
            Some(pb::auth::Method::DeviceToken(t)) => AuthMethod::DeviceToken {
                username: &t.username,
                token: &t.device_token,
            },
            None => {
                self.metrics_mut().auth_failures += 1;
                return vec![
                    Action::send(
                        session_id,
                        envelope(
                            request_id,
                            Kind::AuthResult(pb::AuthResult {
                                ok: false,
                                error_code: pb::ErrorCode::AuthFailed as u32,
                                message: "缺少认证方式".to_owned(),
                                ..Default::default()
                            }),
                        ),
                    ),
                    Action::Close {
                        session: session_id,
                        code: pb::ErrorCode::AuthFailed as u32,
                        message: "缺少认证方式".to_owned(),
                    },
                ];
            }
        };

        // 借用检查：认证器要 &self，而我们在 &mut self 里。先把需要的东西拿出来。
        let result = self.authenticator().authenticate(method);
        match result {
            Ok(outcome) => {
                self.emit(crate::Event::AuthOk {
                    session: session_id,
                    user_id: outcome.user_id,
                });
                if let Some(s) = self.session_mut(session_id) {
                    s.state = SessionState::Ready;
                    // **身份来自用户表，不是连接顺序**。原来这里漏了 user_id，于是
                    // 每个会话拿的是 `open_session` 时的自增序号：先连上来的那个人
                    // 就变成 user 1，不管他是谁。表现是成员表里「名字与 id 对不上」，
                    // 客户端按 `AuthResult.user_id` 找自己时找错人（于是「我在频道 0」）。
                    s.user_id = outcome.user_id;
                    s.name = outcome.name;
                    s.perms = outcome.perms;
                }
                let mut actions = vec![Action::send(
                    session_id,
                    envelope(
                        request_id,
                        Kind::AuthResult(pb::AuthResult {
                            ok: true,
                            user_id: outcome.user_id,
                            perms: outcome.perms,
                            // 首次密码登录时下发，客户端存下来下次免密；
                            // 设备号登录时它本来就是客户端给的，不必再回。
                            device_token: outcome.device_token.clone(),
                            ..Default::default()
                        }),
                    ),
                )];
                actions.push(Action::send(session_id, self.channel_sync_msg()));
                actions
            }
            Err(err) => {
                self.metrics_mut().auth_failures += 1;
                // 只记原因短名：用户名与密码**永不**进日志。
                self.emit(crate::Event::AuthFailed {
                    session: session_id,
                    reason: match err {
                        crate::auth::AuthError::UnknownUser => "unknown_user",
                        crate::auth::AuthError::BadPassword => "bad_password",
                        crate::auth::AuthError::BadToken => "bad_token",
                        crate::auth::AuthError::Unsupported => "unsupported",
                        crate::auth::AuthError::Internal => "internal",
                        crate::auth::AuthError::AnonymousDisabled => "anonymous_disabled",
                        crate::auth::AuthError::TooManyAnonymous => "too_many_anonymous",
                    },
                });
                vec![
                    Action::send(
                        session_id,
                        envelope(
                            request_id,
                            Kind::AuthResult(pb::AuthResult {
                                ok: false,
                                error_code: pb::ErrorCode::AuthFailed as u32,
                                message: err.to_string(),
                                ..Default::default()
                            }),
                        ),
                    ),
                    Action::Close {
                        session: session_id,
                        code: pb::ErrorCode::AuthFailed as u32,
                        message: err.to_string(),
                    },
                ]
            }
        }
    }

    fn on_join_channel(&mut self, session_id: SessionId, join: pb::JoinChannel) -> Vec<Action> {
        if !self.session(session_id).is_some_and(Session::is_ready) {
            self.metrics_mut().control_ignored += 1;
            return Vec::new();
        }
        let channel_id: ChannelId = join.channel_id;
        let Some(channel) = self.channel(channel_id) else {
            return vec![Action::send(
                session_id,
                error_msg(pb::ErrorCode::ChannelNotFound, "频道不存在", false),
            )];
        };
        if channel.is_container() {
            return vec![Action::send(
                session_id,
                error_msg(pb::ErrorCode::PermissionDenied, "这是分组，不能加入", false),
            )];
        }
        if !channel.has_room(self.config().max_channel_users) {
            return vec![Action::send(
                session_id,
                error_msg(pb::ErrorCode::ChannelFull, "频道已满", false),
            )];
        }

        // 先记下旧频道与它的同伴，离开时要通知他们。
        let old_channel = self.session(session_id).and_then(|s| s.channel);
        let old_peers = self.channel_peers(session_id);
        self.place_in_channel(session_id, channel_id);

        let mut actions = Vec::new();
        let Some(user_state) = self.user_state(session_id) else {
            return actions;
        };
        // 新频道的所有人（含自己）都收到 UserJoined。
        let mut targets = self.channel_peers(session_id);
        targets.push(session_id);
        for to in targets {
            actions.push(Action::send(
                to,
                envelope(
                    0,
                    Kind::UserJoined(pb::UserJoined {
                        user: Some(user_state.clone()),
                    }),
                ),
            ));
        }
        // 旧频道的同伴收到 UserLeft（他的 UserState.channel_id 已经变了）。
        for to in old_peers {
            if self.session(to).is_some_and(Session::is_ready) {
                actions.push(Action::send(
                    to,
                    envelope(
                        0,
                        Kind::UserLeft(pb::UserLeft {
                            user_id: user_state.user_id,
                        }),
                    ),
                ));
            }
        }

        // **成员变了，epoch 就变了** —— 必须把新的频道快照推给所有相关的人，
        // 否则客户端会继续用旧的 channel_epoch 发包，服务器一律判「过期」。
        // 这是 P5 联调时才暴露的缺口：原来只在登录时发一次 ChannelSync。
        actions.extend(self.broadcast_channel_state(channel_id));
        if let Some(old) = old_channel {
            actions.extend(self.broadcast_channel_state(old));
        }
        actions
    }

    /// 把一个频道的当前快照推给它的成员 + 自己。
    fn broadcast_channel_state(&self, channel_id: ChannelId) -> Vec<Action> {
        let Some(state) = self.channel_state(channel_id) else {
            return Vec::new();
        };
        let mut targets: Vec<SessionId> = self
            .channels()
            .get(&channel_id)
            .map(|c| {
                c.members
                    .iter()
                    .filter_map(|user| {
                        self.sessions_ready()
                            .find(|s| s.user_id == *user)
                            .map(|s| s.id)
                    })
                    .collect()
            })
            .unwrap_or_default();
        targets.sort_unstable();
        targets.dedup();
        targets
            .into_iter()
            .map(|to| {
                Action::send(
                    to,
                    envelope(
                        0,
                        Kind::ChannelUpdated(pb::ChannelUpdated {
                            channel: Some(state.clone()),
                        }),
                    ),
                )
            })
            .collect()
    }

    fn on_leave_channel(&mut self, session_id: SessionId) -> Vec<Action> {
        if self.session(session_id).and_then(|s| s.channel).is_none() {
            return Vec::new();
        }
        let Some(user_id) = self.session(session_id).map(|s| s.user_id) else {
            return Vec::new();
        };
        let peers = self.channel_peers(session_id);
        self.remove_from_channel(session_id);
        let mut actions = vec![Action::send(
            session_id,
            envelope(0, Kind::UserLeft(pb::UserLeft { user_id })),
        )];
        for to in peers {
            if self.session(to).is_some_and(Session::is_ready) {
                actions.push(Action::send(
                    to,
                    envelope(0, Kind::UserLeft(pb::UserLeft { user_id })),
                ));
            }
        }
        actions
    }

    fn on_set_self_state(&mut self, session_id: SessionId, state: pb::SetSelfState) -> Vec<Action> {
        if let Some(s) = self.session_mut(session_id) {
            s.self_mute = state.self_mute;
            s.self_deaf = state.self_deaf;
        }
        let Some(user_state) = self.user_state(session_id) else {
            return Vec::new();
        };
        let mut targets = self.channel_peers(session_id);
        targets.push(session_id);
        targets
            .into_iter()
            .filter(|to| self.session(*to).is_some_and(Session::is_ready))
            .map(|to| {
                Action::send(
                    to,
                    envelope(
                        0,
                        Kind::UserUpdated(pb::UserUpdated {
                            user: Some(user_state.clone()),
                        }),
                    ),
                )
            })
            .collect()
    }

    /// 一个频道的快照。
    fn channel_state(&self, id: ChannelId) -> Option<pb::ChannelState> {
        let c = self.channel(id)?;
        Some(pb::ChannelState {
            channel_id: c.id,
            parent_id: c.parent,
            name: c.name.clone(),
            position: c.position,
            max_users: c.max_users,
            flags: c.flags,
            description: c.description.clone(),
            channel_epoch: u32::from(c.epoch),
        })
    }

    /// 全量频道 + 用户快照。
    fn channel_sync_msg(&self) -> ControlMessage {
        let channels = self
            .channels()
            .values()
            .filter_map(|c| self.channel_state(c.id))
            .collect();
        let users = self
            .sessions_ready()
            .filter_map(|s| self.user_state(s.id))
            .collect();
        envelope(
            0,
            Kind::ChannelSync(pb::ChannelSync {
                channels,
                users,
                self_channel_id: 0,
            }),
        )
    }

    fn user_state(&self, session_id: SessionId) -> Option<pb::UserState> {
        let s = self.session(session_id)?;
        Some(pb::UserState {
            user_id: s.user_id,
            sender_id: s.sender_id,
            name: s.name.clone(),
            channel_id: s.channel.unwrap_or(0),
            self_mute: s.self_mute,
            self_deaf: s.self_deaf,
            server_mute: s.server_mute,
            server_deaf: s.server_deaf,
            priority_speaker: 0,
            listening_channel_ids: Vec::new(),
        })
    }
}
