//! 频道结构（P7-2）：新建 / 改名 / 移动 / 删除。
//!
//! 这些测试都走真握手（`common::connect`），因为权限判定是「会话就绪 + 位图」两件事，
//! 绕开握手就测不到「没登录的人能不能建频道」这类问题。

mod common;

use common::{BrokenTree, MemTree, connect, roles, send_control};
use protocol::control::proto as pb;
use protocol::control::proto::control_message::Kind;
use server_core::channel::FLAG_CONTAINER;
use server_core::{Action, Server};

/// 一条消息里的错误码（不是错误就返回 `None`）。
fn error_code(actions: &[Action]) -> Option<u32> {
    actions.iter().find_map(|action| match action {
        Action::Send { msg, .. } => match &msg.kind {
            Some(Kind::Error(error)) => Some(error.code),
            _ => None,
        },
        Action::Close { code, .. } => Some(*code),
    })
}

/// 收件人收到的 `ChannelAdded` 里的频道快照。
fn added_for(actions: &[Action], to: u32) -> Option<pb::ChannelState> {
    actions.iter().find_map(|action| match action {
        Action::Send { to: session, msg } if *session == u64::from(to) => match &msg.kind {
            Some(Kind::ChannelAdded(added)) => added.channel.clone(),
            _ => None,
        },
        _ => None,
    })
}

/// 所有人收到的 `ChannelUpdated` 的 id。
fn updated_ids(actions: &[Action]) -> Vec<u32> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::Send { msg, .. } => match &msg.kind {
                Some(Kind::ChannelUpdated(updated)) => {
                    updated.channel.as_ref().map(|c| c.channel_id)
                }
                _ => None,
            },
            Action::Close { .. } => None,
        })
        .collect()
}

/// 收到的 `ChannelRemoved` 的 id（去重，按出现顺序）。
fn removed_ids(actions: &[Action]) -> Vec<u32> {
    let mut ids: Vec<u32> = actions
        .iter()
        .filter_map(|action| match action {
            Action::Send { msg, .. } => match &msg.kind {
                Some(Kind::ChannelRemoved(removed)) => Some(removed.channel_id),
                _ => None,
            },
            Action::Close { .. } => None,
        })
        .collect();
    ids.dedup();
    ids
}

fn create(parent_id: u32, name: &str) -> Kind {
    Kind::CreateChannel(pb::CreateChannel {
        parent_id,
        name: name.to_owned(),
        flags: 0,
        max_users: 0,
    })
}

/// 根级频道的名字，按顺序。
fn root_names(server: &Server) -> Vec<String> {
    server
        .children_of(0)
        .into_iter()
        .filter_map(|id| server.channel(id).map(|c| c.name.clone()))
        .collect()
}

#[test]
fn an_admin_can_create_a_channel_and_everybody_hears_it() {
    let (mut server, _) = roles();
    server.add_channel(0, "大厅");
    let alice = connect(&mut server, "alice", "alice-pw", 1);
    let carol = connect(&mut server, "carol", "carol-pw", 3);

    let actions = send_control(&mut server, alice.id, create(0, "游戏"), 10);
    assert_eq!(error_code(&actions), None, "管理员建频道不该被拒");

    // 两个人**都**收到了 ChannelAdded：频道树是全局的，谁都要看到。
    let mine = added_for(&actions, u32::try_from(alice.id).unwrap()).expect("发起者应当收到");
    let theirs = added_for(&actions, u32::try_from(carol.id).unwrap()).expect("别人也应当收到");
    assert_eq!(mine.name, "游戏");
    assert_eq!(mine.parent_id, 0);
    assert_eq!(mine.position, 1, "根级第二个，接在「大厅」后面");
    assert_eq!(mine, theirs, "所有人看到的是同一个快照");
}

#[test]
fn a_member_cannot_create_a_channel() {
    let (mut server, _) = roles();
    server.add_channel(0, "大厅");
    let carol = connect(&mut server, "carol", "carol-pw", 3);
    let actions = send_control(&mut server, carol.id, create(0, "我的频道"), 10);
    assert_eq!(
        error_code(&actions),
        Some(pb::ErrorCode::PermissionDenied as u32)
    );
    assert_eq!(server.channels().len(), 1, "被拒之后树不能变");
}

#[test]
fn duplicate_names_are_refused_at_the_same_level_but_fine_below_another() {
    let (mut server, _) = roles();
    server.add_channel(0, "大厅");
    let alice = connect(&mut server, "alice", "alice-pw", 1);

    let dup = send_control(&mut server, alice.id, create(0, " 大厅 "), 10);
    assert_eq!(
        error_code(&dup),
        Some(pb::ErrorCode::Conflict as u32),
        "首尾空白去掉之后就是重名"
    );

    // 换个父级就允许（「一队/大厅」和根级的「大厅」不是一回事）。
    let group = server.add_channel(0, "团战");
    let ok = send_control(&mut server, alice.id, create(group, "大厅"), 11);
    assert_eq!(error_code(&ok), None);
}

#[test]
fn bad_names_are_refused_with_a_reason() {
    let (mut server, _) = roles();
    let alice = connect(&mut server, "alice", "alice-pw", 1);
    for bad in ["", "   ", "a/b", "带\n换行"] {
        let actions = send_control(&mut server, alice.id, create(0, bad), 10);
        assert_eq!(
            error_code(&actions),
            Some(pb::ErrorCode::InvalidArgument as u32),
            "{bad:?} 应当被拒"
        );
    }
    let long = "字".repeat(33);
    let actions = send_control(&mut server, alice.id, create(0, &long), 11);
    assert_eq!(
        error_code(&actions),
        Some(pb::ErrorCode::InvalidArgument as u32),
        "33 个字符超长"
    );
    // 32 个字符正好可以。
    let exact = "字".repeat(32);
    let actions = send_control(&mut server, alice.id, create(0, &exact), 12);
    assert_eq!(error_code(&actions), None);
}

#[test]
fn the_tree_stops_at_three_levels() {
    let (mut server, _) = roles();
    let alice = connect(&mut server, "alice", "alice-pw", 1);
    let a = server.add_channel(0, "一");
    let b = server.add_channel(a, "二");
    let c = server.add_channel(b, "三");
    assert_eq!(server.depth_of(c), 3);

    let actions = send_control(&mut server, alice.id, create(c, "四"), 10);
    assert_eq!(
        error_code(&actions),
        Some(pb::ErrorCode::InvalidArgument as u32),
        "第 4 层必须被拒"
    );
}

#[test]
fn moving_a_channel_renumbers_both_parents() {
    let (mut server, _) = roles();
    let a = server.add_channel(0, "A");
    let b = server.add_channel(0, "B");
    let child = server.add_channel(a, "A 的孩子");
    let alice = connect(&mut server, "alice", "alice-pw", 1);

    // 把 A 的孩子挪到根级第 0 位。
    let actions = send_control(
        &mut server,
        alice.id,
        Kind::MoveChannel(pb::MoveChannel {
            channel_id: child,
            parent_id: 0,
            position: 0,
        }),
        10,
    );
    assert_eq!(error_code(&actions), None);
    assert_eq!(root_names(&server), vec!["A 的孩子", "A", "B"]);
    assert!(server.children_of(a).is_empty(), "旧父级里不该还留着");
    // 三个频道的位置都要广播出去（客户端按 id 覆盖，不重发就不同步）。
    let ids = updated_ids(&actions);
    for id in [a, b, child] {
        assert!(ids.contains(&id), "频道 {id} 的新顺序没有广播：{ids:?}");
    }
    // 位置真的重排成 0..n-1。
    for (index, id) in server.children_of(0).into_iter().enumerate() {
        assert_eq!(server.channel(id).unwrap().position, index as u32);
    }
}

#[test]
fn a_channel_cannot_be_moved_into_its_own_subtree() {
    let (mut server, _) = roles();
    let a = server.add_channel(0, "A");
    let b = server.add_channel(a, "B");
    let alice = connect(&mut server, "alice", "alice-pw", 1);

    for target in [a, b] {
        let actions = send_control(
            &mut server,
            alice.id,
            Kind::MoveChannel(pb::MoveChannel {
                channel_id: a,
                parent_id: target,
                position: 0,
            }),
            10,
        );
        assert_eq!(
            error_code(&actions),
            Some(pb::ErrorCode::InvalidArgument as u32),
            "移到 {target} 会成环"
        );
    }
    assert_eq!(server.channel(a).unwrap().parent, 0, "树不能变");
}

#[test]
fn a_move_that_would_go_too_deep_is_refused() {
    let (mut server, _) = roles();
    let a = server.add_channel(0, "一");
    let b = server.add_channel(a, "二");
    server.add_channel(b, "三");
    // 另一棵：一棵有 2 层的子树。
    let x = server.add_channel(0, "X");
    server.add_channel(x, "X 的孩子");
    let alice = connect(&mut server, "alice", "alice-pw", 1);

    // 把「二」（自己 + 孩子 = 2 层）挪到 X 的孩子下面 → 3 层？不，X 的孩子是第 2 层，
    // 加上 2 层 = 第 4 层，超了。
    let deep = server.children_of(x)[0];
    let actions = send_control(
        &mut server,
        alice.id,
        Kind::MoveChannel(pb::MoveChannel {
            channel_id: b,
            parent_id: deep,
            position: 0,
        }),
        10,
    );
    assert_eq!(
        error_code(&actions),
        Some(pb::ErrorCode::InvalidArgument as u32)
    );
    assert_eq!(server.channel(b).unwrap().parent, a, "树不能变");
}

#[test]
fn an_out_of_range_position_is_clamped_to_the_end() {
    let (mut server, _) = roles();
    let a = server.add_channel(0, "A");
    let b = server.add_channel(0, "B");
    let alice = connect(&mut server, "alice", "alice-pw", 1);
    let actions = send_control(
        &mut server,
        alice.id,
        Kind::MoveChannel(pb::MoveChannel {
            channel_id: a,
            parent_id: 0,
            position: 999,
        }),
        10,
    );
    assert_eq!(error_code(&actions), None, "越界不报错，夹到末尾");
    assert_eq!(root_names(&server), vec!["B", "A"]);
    assert_eq!(server.channel(a).unwrap().position, 1);
    let _ = b;
}

#[test]
fn renaming_broadcasts_the_new_name() {
    let (mut server, _) = roles();
    let hall = server.add_channel(0, "大厅");
    let alice = connect(&mut server, "alice", "alice-pw", 1);
    let actions = send_control(
        &mut server,
        alice.id,
        Kind::UpdateChannel(pb::UpdateChannel {
            channel_id: hall,
            name: Some("主厅".to_owned()),
            max_users: Some(8),
            flags: None,
        }),
        10,
    );
    assert_eq!(error_code(&actions), None);
    let channel = server.channel(hall).unwrap();
    assert_eq!(channel.name, "主厅");
    assert_eq!(channel.max_users, 8);
    assert!(updated_ids(&actions).contains(&hall));
}

#[test]
fn an_occupied_channel_cannot_become_a_container() {
    let (mut server, _) = roles();
    let hall = server.add_channel(0, "大厅");
    let carol = connect(&mut server, "carol", "carol-pw", 3);
    common::join_channel(&mut server, carol.id, hall);
    let alice = connect(&mut server, "alice", "alice-pw", 1);

    let actions = send_control(
        &mut server,
        alice.id,
        Kind::UpdateChannel(pb::UpdateChannel {
            channel_id: hall,
            name: None,
            max_users: None,
            flags: Some(FLAG_CONTAINER),
        }),
        10,
    );
    assert_eq!(
        error_code(&actions),
        Some(pb::ErrorCode::Conflict as u32),
        "里面还有人就不能变分组"
    );
    assert!(!server.channel(hall).unwrap().is_container());
}

#[test]
fn deleting_a_channel_with_people_in_it_is_refused() {
    let (mut server, _) = roles();
    let hall = server.add_channel(0, "大厅");
    server.add_channel(0, "挂机");
    let carol = connect(&mut server, "carol", "carol-pw", 3);
    common::join_channel(&mut server, carol.id, hall);
    let alice = connect(&mut server, "alice", "alice-pw", 1);

    let actions = send_control(
        &mut server,
        alice.id,
        Kind::DeleteChannel(pb::DeleteChannel { channel_id: hall }),
        10,
    );
    assert_eq!(
        error_code(&actions),
        Some(pb::ErrorCode::Conflict as u32),
        "有人就拒绝，而不是静默把人挪走"
    );
    assert!(server.channel(hall).is_some());
}

#[test]
fn deleting_a_container_takes_its_children_with_it() {
    let (mut server, _) = roles();
    let group = server.add_channel(0, "常用");
    server.channel_mut(group).unwrap().flags = FLAG_CONTAINER;
    let a = server.add_channel(group, "大厅");
    let b = server.add_channel(group, "团战");
    server.add_channel(0, "挂机");
    let alice = connect(&mut server, "alice", "alice-pw", 1);

    let actions = send_control(
        &mut server,
        alice.id,
        Kind::DeleteChannel(pb::DeleteChannel { channel_id: group }),
        10,
    );
    assert_eq!(error_code(&actions), None);
    assert_eq!(removed_ids(&actions), vec![group, a, b]);
    assert_eq!(server.channels().len(), 1, "只剩「挂机」");
    assert_eq!(root_names(&server), vec!["挂机"]);
}

#[test]
fn the_last_joinable_channel_cannot_be_deleted() {
    let (mut server, _) = roles();
    let group = server.add_channel(0, "常用");
    server.channel_mut(group).unwrap().flags = FLAG_CONTAINER;
    let hall = server.add_channel(group, "大厅");
    let alice = connect(&mut server, "alice", "alice-pw", 1);

    // 删掉唯一能进的频道 → 拒绝。
    let actions = send_control(
        &mut server,
        alice.id,
        Kind::DeleteChannel(pb::DeleteChannel { channel_id: hall }),
        10,
    );
    assert_eq!(error_code(&actions), Some(pb::ErrorCode::Conflict as u32));
    // 连分组一起删也不行（分组本来就不能进）。
    let actions = send_control(
        &mut server,
        alice.id,
        Kind::DeleteChannel(pb::DeleteChannel { channel_id: group }),
        11,
    );
    assert_eq!(error_code(&actions), Some(pb::ErrorCode::Conflict as u32));
    assert_eq!(server.channels().len(), 2);
}

#[test]
fn every_change_is_written_to_the_store() {
    let (mut server, _) = roles();
    let tree = MemTree::empty();
    server.set_channel_store(Box::new(tree.clone()));
    let hall = server.add_channel(0, "大厅");
    server.persist_channels().expect("落盘");
    assert_eq!(tree.names(), vec![(0, 0, "大厅".to_owned())]);

    let alice = connect(&mut server, "alice", "alice-pw", 1);
    send_control(&mut server, alice.id, create(0, "游戏"), 10);
    assert_eq!(
        tree.names(),
        vec![(0, 0, "大厅".to_owned()), (0, 1, "游戏".to_owned())],
        "建完就落盘"
    );

    let game = server.children_of(0)[1];
    send_control(
        &mut server,
        alice.id,
        Kind::UpdateChannel(pb::UpdateChannel {
            channel_id: game,
            name: Some("开黑".to_owned()),
            max_users: None,
            flags: None,
        }),
        11,
    );
    assert!(tree.names().iter().any(|(_, _, name)| name == "开黑"));

    send_control(
        &mut server,
        alice.id,
        Kind::MoveChannel(pb::MoveChannel {
            channel_id: game,
            parent_id: hall,
            position: 0,
        }),
        12,
    );
    assert_eq!(
        tree.names(),
        vec![(0, 0, "大厅".to_owned()), (hall, 0, "开黑".to_owned())]
    );

    send_control(
        &mut server,
        alice.id,
        Kind::DeleteChannel(pb::DeleteChannel { channel_id: game }),
        13,
    );
    assert_eq!(tree.names(), vec![(0, 0, "大厅".to_owned())]);
}

#[test]
fn restoring_reads_the_tree_back_and_continues_numbering() {
    let (mut server, _) = roles();
    let tree = MemTree::with(&[(1, 0, "常用", 0), (2, 1, "大厅", 0), (7, 0, "挂机", 1)]);
    server.set_channel_store(Box::new(tree.clone()));
    assert_eq!(server.restore_channels().expect("读"), 3);

    assert_eq!(root_names(&server), vec!["常用", "挂机"]);
    assert_eq!(server.children_of(1), vec![2]);
    assert_eq!(server.depth_of(2), 2);

    // 发号器要接上库里最大的 id：新频道不能撞上 7。
    let alice = connect(&mut server, "alice", "alice-pw", 1);
    let actions = send_control(&mut server, alice.id, create(0, "新的"), 10);
    let added = added_for(&actions, u32::try_from(alice.id).unwrap()).expect("应当收到");
    assert!(added.channel_id > 7, "新 id 必须大于库里的最大值");
}

#[test]
fn a_full_disk_is_reported_instead_of_silently_lost() {
    let (mut server, _) = roles();
    server.set_channel_store(Box::new(BrokenTree));
    server.add_channel(0, "大厅");
    let alice = connect(&mut server, "alice", "alice-pw", 1);
    let actions = send_control(&mut server, alice.id, create(0, "游戏"), 10);
    // 改动生效了（广播里有），但发起者另外收到一条「没写进磁盘」。
    assert!(
        added_for(&actions, u32::try_from(alice.id).unwrap()).is_some(),
        "内存里的树应当已经改了"
    );
    assert_eq!(
        error_code(&actions),
        Some(pb::ErrorCode::Internal as u32),
        "落盘失败要明说"
    );
    assert_eq!(server.channels().len(), 2);
}

#[test]
fn creating_channels_counts_toward_the_metrics() {
    let (mut server, _) = roles();
    let alice = connect(&mut server, "alice", "alice-pw", 1);
    send_control(&mut server, alice.id, create(0, "一"), 10);
    send_control(&mut server, alice.id, create(0, "二"), 11);
    assert_eq!(server.metrics().channel_changes, 2);
    // 事件里只有 id 与动作名，没有频道名（频道名是用户内容）。
    let events = server.drain_events();
    let changed = events
        .iter()
        .filter(|event| event.kind() == "channel_changed")
        .count();
    assert_eq!(changed, 2);
    for event in &events {
        assert!(!event.to_json().contains('一'), "事件里不该有频道名");
    }
}

#[test]
fn the_owner_and_an_admin_both_can_manage_the_tree() {
    let (mut server, _) = roles();
    let alice = connect(&mut server, "alice", "alice-pw", 1);
    let bob = connect(&mut server, "bob", "bob-pw", 2);
    let a = send_control(&mut server, alice.id, create(0, "服主建的"), 10);
    assert_eq!(error_code(&a), None);
    let b = send_control(&mut server, bob.id, create(0, "管理员建的"), 11);
    assert_eq!(error_code(&b), None);
    assert_eq!(server.channels().len(), 2);
}
