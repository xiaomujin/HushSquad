//! 权限位与角色（ADR-0016）。
//!
//! 权限是**账号级**的全局位图，不是每频道一份 ACL。位定义与
//! `docs/proto/hushsquad-control-v1.proto` 的 `Perm` 一一对应，有测试钉住两边一致。
//!
//! # 为什么角色是「命名组合」而不是枚举
//!
//! 库里存的是位图（`users.perms`）。角色只是位图的一个名字，用来让界面说人话：
//! 「管理员」比「255」好读。好处是三件事：
//!
//! - 加一个角色不用改库结构，也不用数据迁移；
//! - 手工改库改出个奇怪组合时，界面显示「自定义」而不是崩掉；
//! - 将来加位（比如按频道 ACL）时，老的位含义不变。
//!
//! # 为什么四个角色的位是**嵌套**的
//!
//! 成员 ⊂ 协管 ⊂ 管理员 ⊂ 服主。这条让「不能操作权限比自己高的人」退化成一次
//! 子集判断（[`can_act_on`]），不需要再维护一张「谁大谁小」的表。如果将来出现
//! 互不包含的组合（自定义位），子集判断照样成立：一个位都不比别人多的人，动不了别人。

/// 进频道。所有登录用户都有。
pub const JOIN_CHANNEL: u64 = 1;
/// 把**别人**移到别的频道。
pub const MOVE_ANYONE: u64 = 2;
/// 踢出服务器（断开会话）。
pub const KICK: u64 = 4;
/// 封禁。v1 不做，位保留。
pub const BAN: u64 = 8;
/// 新建频道。
pub const CREATE_CHANNEL: u64 = 16;
/// 改名 / 删除 / 排序 / 人数上限 / 设为分组。
pub const MANAGE_CHANNEL: u64 = 32;
/// 分配角色、查看账号列表。
pub const ADMIN: u64 = 64;
/// 服务器静音别人。
pub const MUTE_OTHERS: u64 = 128;
/// 服主：唯一，可转移。
pub const OWNER: u64 = 256;

/// 成员：只能进频道。
pub const MEMBER: u64 = JOIN_CHANNEL;
/// 协管：能管人（移动 / 踢 / 服务器静音），**不能**动频道结构。
pub const MODERATOR: u64 = MEMBER | MOVE_ANYONE | KICK | MUTE_OTHERS;
/// 管理员：全部低位。
pub const ADMIN_ROLE: u64 = MODERATOR | BAN | CREATE_CHANNEL | MANAGE_CHANNEL | ADMIN;
/// 服主：管理员 + 唯一的 `OWNER` 位。
pub const OWNER_ROLE: u64 = ADMIN_ROLE | OWNER;

/// 内置角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// 服主。
    Owner,
    /// 管理员。
    Admin,
    /// 协管。
    Moderator,
    /// 成员。
    Member,
}

impl Role {
    /// 角色展开成位图。
    #[must_use]
    pub const fn perms(self) -> u64 {
        match self {
            Self::Owner => OWNER_ROLE,
            Self::Admin => ADMIN_ROLE,
            Self::Moderator => MODERATOR,
            Self::Member => MEMBER,
        }
    }

    /// 界面用的名字。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Owner => "服主",
            Self::Admin => "管理员",
            Self::Moderator => "协管",
            Self::Member => "成员",
        }
    }

    /// 全部角色（界面下拉用）。
    pub const ALL: [Self; 4] = [Self::Owner, Self::Admin, Self::Moderator, Self::Member];
}

/// 位图 → 角色名。组合不在内置角色里时返回 `None`（界面显示「自定义」）。
#[must_use]
pub fn role_of(perms: u64) -> Option<Role> {
    Role::ALL.into_iter().find(|role| role.perms() == perms)
}

/// 名字 → 角色。界面与命令行共用。
pub fn parse_role(text: &str) -> Result<Role, String> {
    match text.trim() {
        "owner" | "服主" => Ok(Role::Owner),
        "admin" | "管理员" => Ok(Role::Admin),
        "moderator" | "mod" | "协管" => Ok(Role::Moderator),
        "member" | "成员" => Ok(Role::Member),
        other => Err(format!(
            "角色 {other:?} 不认识（可选：owner / admin / moderator / member）"
        )),
    }
}

/// `perms` 是否包含 `need` 里的**全部**位。
#[must_use]
pub const fn has(perms: u64, need: u64) -> bool {
    perms & need == need
}

/// 能不能对这个人做破坏性操作（踢 / 服务器静音 / 降权）。
///
/// 规则是「**操作者的位必须是目标的超集**」：管理员动不了服主，协管动不了管理员，
/// 谁都动不了自己权限更高的人。这一条挡住了「管理员踢服主」。
#[must_use]
pub const fn can_act_on(actor: u64, target: u64) -> bool {
    actor & target == target
}

/// 授予的位必须是自己的子集。
///
/// 这一条挡住了提权：管理员造不出服主，也给自己加不了权限——给自己加位时
/// 「授予 ⊄ 自己」，直接被拒。
#[must_use]
pub const fn can_grant(actor: u64, granted: u64) -> bool {
    has(actor, granted)
}

/// 位图的可读描述（日志与命令行用，如 `join|kick|admin`）。
#[must_use]
pub fn describe(perms: u64) -> String {
    if let Some(role) = role_of(perms) {
        return format!("{}({perms})", role.label());
    }
    const BITS: [(u64, &str); 9] = [
        (JOIN_CHANNEL, "join"),
        (MOVE_ANYONE, "move"),
        (KICK, "kick"),
        (BAN, "ban"),
        (CREATE_CHANNEL, "create"),
        (MANAGE_CHANNEL, "manage"),
        (ADMIN, "admin"),
        (MUTE_OTHERS, "mute"),
        (OWNER, "owner"),
    ];
    let names: Vec<&str> = BITS
        .iter()
        .filter(|(bit, _)| perms & bit != 0)
        .map(|(_, name)| *name)
        .collect();
    if names.is_empty() {
        format!("无({perms})")
    } else {
        format!("{}({perms})", names.join("|"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::control::proto::Perm;

    #[test]
    fn the_bit_values_match_the_protocol_enum() {
        // 位定义在 proto 里，常量在 Rust 里。两边必须一致，否则线上传的位图会被读错意思。
        for (constant, proto) in [
            (JOIN_CHANNEL, Perm::JoinChannel),
            (MOVE_ANYONE, Perm::MoveAnyone),
            (KICK, Perm::Kick),
            (BAN, Perm::Ban),
            (CREATE_CHANNEL, Perm::CreateChannel),
            (MANAGE_CHANNEL, Perm::ManageChannel),
            (ADMIN, Perm::Admin),
            (MUTE_OTHERS, Perm::MuteOthers),
            (OWNER, Perm::Owner),
        ] {
            assert_eq!(constant, proto as u64, "位值与 proto 不一致");
        }
    }

    #[test]
    fn roles_are_nested() {
        // 嵌套是 can_act_on 能退化成子集判断的前提。
        assert!(has(MODERATOR, MEMBER), "协管应当包含成员的全部位");
        assert!(has(ADMIN_ROLE, MODERATOR), "管理员应当包含协管的全部位");
        assert!(has(OWNER_ROLE, ADMIN_ROLE), "服主应当包含管理员的全部位");
        // 协管不能动频道结构。
        assert!(!has(MODERATOR, MANAGE_CHANNEL));
        assert!(!has(MODERATOR, CREATE_CHANNEL));
        // 管理员不是服主。
        assert!(!has(ADMIN_ROLE, OWNER));
    }

    #[test]
    fn an_admin_cannot_act_on_the_owner() {
        assert!(can_act_on(ADMIN_ROLE, MEMBER));
        assert!(can_act_on(ADMIN_ROLE, MODERATOR));
        assert!(!can_act_on(ADMIN_ROLE, OWNER_ROLE), "管理员不能踢服主");
        assert!(!can_act_on(MODERATOR, ADMIN_ROLE), "协管不能踢管理员");
        assert!(can_act_on(OWNER_ROLE, ADMIN_ROLE));
    }

    #[test]
    fn nobody_can_grant_more_than_they_have() {
        assert!(can_grant(ADMIN_ROLE, MEMBER));
        assert!(can_grant(ADMIN_ROLE, ADMIN_ROLE));
        assert!(!can_grant(ADMIN_ROLE, OWNER_ROLE), "管理员造不出服主");
        // 给自己加位：授予 ⊄ 自己，被拒。
        assert!(!can_grant(ADMIN_ROLE, ADMIN_ROLE | OWNER));
        assert!(can_grant(OWNER_ROLE, OWNER_ROLE), "服主可以转移服主位");
    }

    #[test]
    fn role_names_round_trip() {
        for role in Role::ALL {
            assert_eq!(role_of(role.perms()), Some(role));
            assert_eq!(parse_role(role.label()).unwrap(), role);
        }
        // 组合不在内置角色里 → 自定义。
        assert_eq!(role_of(JOIN_CHANNEL | KICK), None);
        assert!(parse_role("皇帝").is_err());
    }

    #[test]
    fn describe_says_something_readable() {
        assert_eq!(describe(ADMIN_ROLE), "管理员(255)");
        assert_eq!(describe(JOIN_CHANNEL | KICK), "join|kick(5)");
        assert_eq!(describe(0), "无(0)");
    }
}
