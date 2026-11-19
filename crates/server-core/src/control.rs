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
use crate::channel::{ChannelId, UserId};
use crate::perms;
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

/// 带 `request_id` 的错误回复。
///
/// 管理类请求必须回带同一个 `request_id`：客户端靠它把「失败」对回到自己刚点的那个操作上，
/// 否则界面上只能显示一条不知道从哪来的错误。
fn error_reply(request_id: u64, code: pb::ErrorCode, message: &str) -> ControlMessage {
    envelope(
        request_id,
        Kind::Error(pb::Error {
            code: code as u32,
            message: message.to_owned(),
            fatal: false,
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
            Some(Kind::MoveUser(req)) => self.on_move_user(session_id, request_id, req),
            Some(Kind::SetUserState(req)) => self.on_set_user_state(session_id, request_id, req),
            Some(Kind::KickUser(req)) => self.on_kick_user(session_id, request_id, req),
            Some(Kind::ListUsers(_)) => self.on_list_users(session_id, request_id),
            Some(Kind::SetUserPerms(req)) => self.on_set_user_perms(session_id, request_id, req),
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

        self.apply_channel_move(session_id, channel_id)
    }

    /// 把一个会话放进频道，并把「谁进了谁出了」广播出去。
    ///
    /// **调用方负责校验**（频道存在、不是分组、没满员、有权限）。抽出来是因为
    /// 自己进频道（`JoinChannel`）和把别人移过去（`MoveUser`）之后的动作**必须一模一样**——
    /// 两份实现迟早会漏掉「广播 channel_epoch」这种一句话。
    fn apply_channel_move(&mut self, session_id: SessionId, channel_id: ChannelId) -> Vec<Action> {
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

    // ------------------------------------------------------------ 管理与权限（ADR-0016）

    /// 管理类请求的公共前置：会话就绪 + 有权限。
    ///
    /// 返回 `Some(actions)` 表示「到此为止」；`None` 表示可以继续。
    fn require(
        &mut self,
        session_id: SessionId,
        request_id: u64,
        need: u64,
    ) -> Option<Vec<Action>> {
        let (ready, actor_perms) = match self.session(session_id) {
            Some(session) => (session.is_ready(), session.perms),
            None => return Some(Vec::new()),
        };
        if !ready {
            self.metrics_mut().control_ignored += 1;
            return Some(Vec::new());
        }
        if !perms::has(actor_perms, need) {
            self.metrics_mut().permission_denied += 1;
            return Some(vec![Action::send(
                session_id,
                error_reply(
                    request_id,
                    pb::ErrorCode::PermissionDenied,
                    "没有权限做这件事（需要管理员给你分配角色）",
                ),
            )]);
        }
        None
    }

    /// 某个账号当前在线的那条会话。
    fn session_of_user(&self, user_id: UserId) -> Option<SessionId> {
        self.sessions_ready()
            .find(|session| session.user_id == user_id)
            .map(|session| session.id)
    }

    /// 把一个 `UserState` 推给这些人（去重，且只推给就绪的会话）。
    fn push_user_state(&self, targets: &[SessionId], user_state: &pb::UserState) -> Vec<Action> {
        let mut targets: Vec<SessionId> = targets
            .iter()
            .copied()
            .filter(|to| self.session(*to).is_some_and(Session::is_ready))
            .collect();
        targets.sort_unstable();
        targets.dedup();
        targets
            .into_iter()
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

    /// 所有带 `ADMIN` 位的会话（管理面板要跟着变）。
    fn admin_sessions(&self) -> Vec<SessionId> {
        self.sessions_ready()
            .filter(|session| perms::has(session.perms, perms::ADMIN))
            .map(|session| session.id)
            .collect()
    }

    /// 把别人移到另一个频道。需要 `MOVE_ANYONE`。
    fn on_move_user(
        &mut self,
        session_id: SessionId,
        request_id: u64,
        req: pb::MoveUser,
    ) -> Vec<Action> {
        if let Some(actions) = self.require(session_id, request_id, perms::MOVE_ANYONE) {
            return actions;
        }
        let Some(target) = self.session_of_user(req.user_id) else {
            return vec![Action::send(
                session_id,
                error_reply(request_id, pb::ErrorCode::InvalidArgument, "这个人不在线"),
            )];
        };
        let actor_perms = self.session(session_id).map_or(0, |s| s.perms);
        let target_perms = self.session(target).map_or(0, |s| s.perms);
        if !perms::can_act_on(actor_perms, target_perms) {
            self.metrics_mut().permission_denied += 1;
            return vec![Action::send(
                session_id,
                error_reply(
                    request_id,
                    pb::ErrorCode::PermissionDenied,
                    "不能移动权限比你高的人",
                ),
            )];
        }
        let Some(channel) = self.channel(req.channel_id) else {
            return vec![Action::send(
                session_id,
                error_reply(request_id, pb::ErrorCode::ChannelNotFound, "频道不存在"),
            )];
        };
        if channel.is_container() {
            return vec![Action::send(
                session_id,
                error_reply(
                    request_id,
                    pb::ErrorCode::PermissionDenied,
                    "这是分组，不能加入",
                ),
            )];
        }
        if !channel.has_room(self.config().max_channel_users) {
            return vec![Action::send(
                session_id,
                error_reply(request_id, pb::ErrorCode::ChannelFull, "频道已满"),
            )];
        }
        if self.session(target).and_then(|s| s.channel) == Some(req.channel_id) {
            // 已经在那儿了。不报错：右键点错一次不该弹窗。
            return Vec::new();
        }
        self.apply_channel_move(target, req.channel_id)
    }

    /// 改别人的服务器侧状态（被管理员静音 / 闭麦）。需要 `MUTE_OTHERS`。
    fn on_set_user_state(
        &mut self,
        session_id: SessionId,
        request_id: u64,
        req: pb::SetUserState,
    ) -> Vec<Action> {
        if let Some(actions) = self.require(session_id, request_id, perms::MUTE_OTHERS) {
            return actions;
        }
        let Some(target) = self.session_of_user(req.user_id) else {
            return vec![Action::send(
                session_id,
                error_reply(request_id, pb::ErrorCode::InvalidArgument, "这个人不在线"),
            )];
        };
        let actor_perms = self.session(session_id).map_or(0, |s| s.perms);
        let target_perms = self.session(target).map_or(0, |s| s.perms);
        if !perms::can_act_on(actor_perms, target_perms) {
            self.metrics_mut().permission_denied += 1;
            return vec![Action::send(
                session_id,
                error_reply(
                    request_id,
                    pb::ErrorCode::PermissionDenied,
                    "不能静音权限比你高的人",
                ),
            )];
        }
        if let Some(s) = self.session_mut(target) {
            if let Some(mute) = req.server_mute {
                s.server_mute = mute;
            }
            if let Some(deaf) = req.server_deaf {
                s.server_deaf = deaf;
            }
        }
        let Some(user_state) = self.user_state(target) else {
            return Vec::new();
        };
        // 推给：他所在频道的人 + 他自己 + 动手的管理员（他可能不在同一个频道）。
        let mut targets = self.channel_peers(target);
        targets.push(target);
        targets.push(session_id);
        self.push_user_state(&targets, &user_state)
    }

    /// 踢出服务器。需要 `KICK`。
    fn on_kick_user(
        &mut self,
        session_id: SessionId,
        request_id: u64,
        req: pb::KickUser,
    ) -> Vec<Action> {
        if let Some(actions) = self.require(session_id, request_id, perms::KICK) {
            return actions;
        }
        let Some(target) = self.session_of_user(req.user_id) else {
            return vec![Action::send(
                session_id,
                error_reply(request_id, pb::ErrorCode::InvalidArgument, "这个人不在线"),
            )];
        };
        if target == session_id {
            return vec![Action::send(
                session_id,
                error_reply(
                    request_id,
                    pb::ErrorCode::InvalidArgument,
                    "不能踢自己（想退出请断开连接）",
                ),
            )];
        }
        let actor_perms = self.session(session_id).map_or(0, |s| s.perms);
        let target_perms = self.session(target).map_or(0, |s| s.perms);
        if !perms::can_act_on(actor_perms, target_perms) {
            self.metrics_mut().permission_denied += 1;
            return vec![Action::send(
                session_id,
                error_reply(
                    request_id,
                    pb::ErrorCode::PermissionDenied,
                    "不能踢权限比你高的人",
                ),
            )];
        }
        // 原因只在**给被踢的人看的那条消息**里，不进日志（日志只记事件与 id）。
        let reason = if req.reason.is_empty() {
            "被管理员移出了服务器".to_owned()
        } else {
            format!("被管理员移出了服务器：{}", req.reason)
        };
        self.emit(crate::Event::SessionKilled {
            session: target,
            reason: crate::KillReason::Kicked,
        });
        self.metrics_mut().kicks += 1;
        vec![
            Action::send(
                target,
                envelope(
                    request_id,
                    Kind::Error(pb::Error {
                        code: pb::ErrorCode::Kicked as u32,
                        message: reason.clone(),
                        fatal: true,
                    }),
                ),
            ),
            Action::Close {
                session: target,
                code: pb::ErrorCode::Kicked as u32,
                message: reason,
            },
        ]
    }

    /// 账号列表（含离线的）。需要 `ADMIN`。
    fn on_list_users(&mut self, session_id: SessionId, request_id: u64) -> Vec<Action> {
        if let Some(actions) = self.require(session_id, request_id, perms::ADMIN) {
            return actions;
        }
        let Some(admin) = self.user_admin() else {
            return vec![Action::send(
                session_id,
                error_reply(
                    request_id,
                    pb::ErrorCode::Internal,
                    "这个服务器没有接账号管理",
                ),
            )];
        };
        let entries = match admin.list_users() {
            Ok(entries) => entries,
            Err(error) => {
                return vec![Action::send(
                    session_id,
                    error_reply(request_id, pb::ErrorCode::Internal, &error.to_string()),
                )];
            }
        };
        let online: Vec<UserId> = self.sessions_ready().map(|s| s.user_id).collect();
        let users = entries
            .into_iter()
            .map(|entry| pb::UserEntry {
                user_id: entry.user_id,
                username: entry.username,
                perms: entry.perms,
                anonymous: entry.anonymous,
                online: online.contains(&entry.user_id),
            })
            .collect();
        vec![Action::send(
            session_id,
            envelope(request_id, Kind::UserList(pb::UserList { users })),
        )]
    }

    /// 改某个账号的权限位。需要 `ADMIN`，且授予的位必须是自己的子集。
    fn on_set_user_perms(
        &mut self,
        session_id: SessionId,
        request_id: u64,
        req: pb::SetUserPerms,
    ) -> Vec<Action> {
        if let Some(actions) = self.require(session_id, request_id, perms::ADMIN) {
            return actions;
        }
        let actor_perms = self.session(session_id).map_or(0, |s| s.perms);
        // 1. 防提权：授予的位必须是自己的子集（给自己加位也走这条）。
        if !perms::can_grant(actor_perms, req.perms) {
            self.metrics_mut().permission_denied += 1;
            return vec![Action::send(
                session_id,
                error_reply(
                    request_id,
                    pb::ErrorCode::PermissionDenied,
                    "不能授予自己没有的权限（服主位只能由服主转移）",
                ),
            )];
        }
        let Some(admin) = self.user_admin() else {
            return vec![Action::send(
                session_id,
                error_reply(
                    request_id,
                    pb::ErrorCode::Internal,
                    "这个服务器没有接账号管理",
                ),
            )];
        };
        let Ok(mut users) = admin.list_users() else {
            return vec![Action::send(
                session_id,
                error_reply(request_id, pb::ErrorCode::Internal, "读账号库失败"),
            )];
        };
        let Some(target) = users
            .iter()
            .find(|entry| entry.user_id == req.user_id)
            .cloned()
        else {
            return vec![Action::send(
                session_id,
                error_reply(request_id, pb::ErrorCode::InvalidArgument, "没有这个账号"),
            )];
        };
        // 2. 不能改权限比自己高的人（降级同僚要走自己的权限范围）。
        if !perms::can_act_on(actor_perms, target.perms) {
            self.metrics_mut().permission_denied += 1;
            return vec![Action::send(
                session_id,
                error_reply(
                    request_id,
                    pb::ErrorCode::PermissionDenied,
                    "不能改权限比你高的人",
                ),
            )];
        }

        // 3. 服主位：要么原子转移，要么拒绝。
        let mut changes: Vec<(UserId, u64)> = Vec::new();
        let wants_owner = req.perms & perms::OWNER != 0;
        let is_owner = target.perms & perms::OWNER != 0;
        if wants_owner && !is_owner {
            // 转移：先把旧服主的位清掉。旧服主失去 OWNER 后仍是管理员。
            for entry in &users {
                if entry.user_id != req.user_id && entry.perms & perms::OWNER != 0 {
                    changes.push((entry.user_id, entry.perms & !perms::OWNER));
                }
            }
        } else if is_owner && !wants_owner {
            return vec![Action::send(
                session_id,
                error_reply(
                    request_id,
                    pb::ErrorCode::Conflict,
                    "服务器必须有一个服主；要换人请把服主位直接给新的人",
                ),
            )];
        }
        changes.push((req.user_id, req.perms));

        // 4. 不能把最后一个管理员改掉（服主算管理员，所以正常情况下这条不会触发，
        //    但「把唯一的管理员降成成员」必须被挡住，否则服务器就没人能管了）。
        for (user_id, new_perms) in &changes {
            if let Some(entry) = users.iter_mut().find(|entry| entry.user_id == *user_id) {
                entry.perms = *new_perms;
            }
        }
        if !users
            .iter()
            .any(|entry| perms::has(entry.perms, perms::ADMIN))
        {
            return vec![Action::send(
                session_id,
                error_reply(
                    request_id,
                    pb::ErrorCode::Conflict,
                    "不能把最后一个管理员降级（服务器会没人能管）",
                ),
            )];
        }

        // 5. 落库 + 更新在线会话 + 通知。
        for (user_id, new_perms) in &changes {
            if let Err(error) = admin.set_perms(*user_id, *new_perms) {
                return vec![Action::send(
                    session_id,
                    error_reply(request_id, pb::ErrorCode::Internal, &error.to_string()),
                )];
            }
        }
        let mut actions = Vec::new();
        for (user_id, new_perms) in &changes {
            for target_session in self
                .sessions_ready()
                .map(|s| (s.id, s.user_id))
                .collect::<Vec<_>>()
            {
                if target_session.1 == *user_id
                    && let Some(session) = self.session_mut(target_session.0)
                {
                    session.perms = *new_perms;
                }
            }
            self.emit(crate::Event::PermsChanged { user_id: *user_id });
            // 只发给本人和所有管理员（v1 不广播别人的权限）。
            let mut recipients = self.admin_sessions();
            recipients.extend(
                self.sessions_ready()
                    .filter(|s| s.user_id == *user_id)
                    .map(|s| s.id),
            );
            recipients.sort_unstable();
            recipients.dedup();
            for to in recipients {
                actions.push(Action::send(
                    to,
                    envelope(
                        0,
                        Kind::PermsChanged(pb::PermsChanged {
                            user_id: *user_id,
                            perms: *new_perms,
                        }),
                    ),
                ));
            }
        }
        actions
    }
}
