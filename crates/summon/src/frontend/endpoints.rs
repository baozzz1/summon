//! 飞书 HTTP 端点清单 —— **URL 只在这个文件里构造**。
//!
//! 为什么单拉一个文件:手拼 URL 漏掉 query 参数时,飞书**不报错**,而是静默返回一份信息更少的
//! 响应。`fetch_message` 就曾漏掉 `card_msg_content_type=raw_card_content`,导致被引用的卡片被
//! 降级成扁平结构(样式丢失、`<` 变成 `&#60;`、一句话按样式边界拆成多行),整条链路没有任何
//! 报错,直到 owner 发现引用进 prompt 的内容对不上原文。
//!
//! 规矩:
//! 1. 每个端点一个纯函数,**必带的 query 参数写死在函数体里**,不由调用方传。
//! 2. 每个必带参数在文件末尾的 `required_query_params_are_pinned` 里各有一条断言。
//! 3. `client.rs` 里不允许再出现 `open-apis` 字面量 —— 末尾的哨兵测试守着这条线。
//!
//! 加新端点时:在这里加函数 → 在 `required_query_params_are_pinned` 里钉死它的必带参数
//! (没有必带参数就不用加断言,但要在 `paths_are_wellformed` 里留一条路径断言)。

/// 当前应用已获授权的权限列表。无必带参数。
pub fn application_scopes(base: &str) -> String {
    format!("{base}/open-apis/application/v6/scopes")
}

/// 查应用信息(拿应用所有者),owner_id 按 open_id 返回。**必带** `lang`(接口必填)与
/// `user_id_type=open_id`:不带 id 类型时 owner_id 按默认类型返回,与入站的 open_id 对不上。
pub fn application_info_open_id(base: &str, app_id: &str) -> String {
    format!("{base}/open-apis/application/v6/applications/{app_id}?lang=zh_cn&user_id_type=open_id")
}

/// 同 [`application_info_open_id`],owner_id 按跨 app 稳定的 union_id 返回。
pub fn application_info_union_id(base: &str, app_id: &str) -> String {
    format!(
        "{base}/open-apis/application/v6/applications/{app_id}?lang=zh_cn&user_id_type=union_id"
    )
}

/// 机器人自身信息(拿 bot 的 open_id)。无必带参数。
pub fn bot_info(base: &str) -> String {
    format!("{base}/open-apis/bot/v3/info")
}

pub(crate) fn document_metadata(base: &str) -> String {
    format!("{base}/open-apis/drive/v1/metas/batch_query?user_id_type=open_id")
}

pub(crate) fn document_comments(base: &str, token: &str, typ: &str) -> String {
    format!("{base}/open-apis/drive/v1/files/{token}/comments/batch_query?file_type={typ}&user_id_type=open_id")
}

pub(crate) fn document_replies(base: &str, token: &str, typ: &str, comment: &str) -> String {
    format!("{base}/open-apis/drive/v1/files/{token}/comments/{comment}/replies?file_type={typ}&user_id_type=open_id&page_size=100")
}

pub(crate) fn document_text(base: &str, token: &str) -> String {
    format!("{base}/open-apis/docx/v1/documents/{token}/raw_content?lang=0")
}

/// 换取 tenant_access_token。无必带参数(凭据走 body)。
pub fn tenant_access_token(base: &str) -> String {
    format!("{base}/open-apis/auth/v3/tenant_access_token/internal")
}

/// 给某条消息贴表情。无必带参数。
pub fn reactions(base: &str, message_id: &str) -> String {
    format!("{base}/open-apis/im/v1/messages/{message_id}/reactions")
}

/// 撤掉某条消息上的一个表情。无必带参数。
pub fn reaction_delete(base: &str, message_id: &str, reaction_id: &str) -> String {
    format!("{base}/open-apis/im/v1/messages/{message_id}/reactions/{reaction_id}")
}

/// 查用户信息(名字 / union_id)。**必带** `user_id_type=open_id`,否则飞书按默认 id 类型解析、查不到人。
pub fn user_info(base: &str, open_id: &str) -> String {
    format!("{base}/open-apis/contact/v3/users/{open_id}?user_id_type=open_id")
}

/// 按 union_id 查用户(拿**本 app 命名空间**的 open_id)。**必带** `user_id_type=union_id`,
/// 否则飞书把 on_ 串按默认 open_id 解析、查不到人。
pub fn user_info_by_union(base: &str, union_id: &str) -> String {
    format!("{base}/open-apis/contact/v3/users/{union_id}?user_id_type=union_id")
}

/// 查群信息(群名)。无必带参数。
pub fn chat_info(base: &str, chat_id: &str) -> String {
    format!("{base}/open-apis/im/v1/chats/{chat_id}")
}

/// 建群(bot 身份;`im.chats.create` 仅支持 tenant token)。**必带** `user_id_type=open_id`,
/// 否则 body 里 `owner_id`/`user_id_list` 按默认 id 类型解析——拉不进人且飞书不报错;
/// `set_bot_manager=false` 一并钉死:群主是 owner,创建 bot 不该顺手给自己加管理员。
pub fn create_chat(base: &str) -> String {
    format!("{base}/open-apis/im/v1/chats?user_id_type=open_id&set_bot_manager=false")
}

/// 列群成员(一页)。**必带** `member_id_type=open_id`,否则返回的 member_id 不是 open_id、
/// 对不上发信人。
///
/// **为什么要它**:`contact/v3/users` 查不到 bot —— bot 不是通讯录用户,拿 bot 的 open_id 去查
/// 必然失败,发信人名字就永远退化成「用户」。群成员列表里 bot 和人都在,且 bot 项带 `app_id`,
/// 据此区分。一次调用能把整页 bot 的名字全缓存下来,不必逐个查。
///
/// `page_token` 由上一页响应的 `data.page_token` 给出(首页传 `None`)。**分页必须跟到底**:
/// 单页上限 100,超过 100 人的群里,不翻页就等于「排在 100 名之后的人永远没有名字」——
/// 而飞书对此不报错,只是少还几条,故障形态是静默的。
/// 本 Bot 所在的群列表。tenant token 下返回的就是本 app 这个 Bot 的在群关系,
/// 所以「目标群在不在列表里」直接回答「Bot 在不在这个群」,不必再比对成员身份。
pub fn bot_chats(base: &str, page_token: Option<&str>) -> String {
    let base_url = format!("{base}/open-apis/im/v1/chats?page_size=100");
    match page_token.filter(|token| !token.is_empty()) {
        Some(token) => format!("{base_url}&page_token={token}"),
        None => base_url,
    }
}

pub fn chat_members(base: &str, chat_id: &str, page_token: Option<&str>) -> String {
    let base_url = format!(
        "{base}/open-apis/im/v1/chats/{chat_id}/members?member_id_type=open_id&page_size=100"
    );
    match page_token.filter(|token| !token.is_empty()) {
        Some(token) => format!("{base_url}&page_token={token}"),
        None => base_url,
    }
}

/// 回复某条消息。进 thread 的唯一途径 —— 飞书的 `receive_id_type` 只认 chat/user,没有
/// 「发进某个 thread」这种收件人;要让消息落进 thread,只能回复 thread 里的一条消息。
pub fn reply_message(base: &str, message_id: &str) -> String {
    format!("{base}/open-apis/im/v1/messages/{message_id}/reply")
}

/// 发消息。**必带** `receive_id_type`,否则飞书不知道 receive_id 是 chat_id 还是 open_id。
pub fn send_message(base: &str, receive_id_type: &str) -> String {
    format!("{base}/open-apis/im/v1/messages?receive_id_type={receive_id_type}")
}

/// 下载消息里的图片/文件资源。**必带** `type`(image / file),否则飞书不知道按哪种取。
pub fn message_resource(base: &str, message_id: &str, file_key: &str, typ: &str) -> String {
    format!("{base}/open-apis/im/v1/messages/{message_id}/resources/{file_key}?type={typ}")
}

/// 上传图片拿 `image_key`(multipart:`image_type=message` + `image=<bytes>`)。
/// 与 `message_resource`(下载)是反方向的一对。
pub fn upload_image(base: &str) -> String {
    format!("{base}/open-apis/im/v1/images")
}

/// 上传文件拿 `file_key`(multipart:`file_type` + `file_name` + `file=<bytes>`)。
pub fn upload_file(base: &str) -> String {
    format!("{base}/open-apis/im/v1/files")
}

/// 按 id 拉回一条消息(引用回复时取被引用消息)。
///
/// **必带** `card_msg_content_type=raw_card_content`:不带它,interactive 卡片会被飞书降级成扁平
/// post 结构——样式丢失、`<` 编成 `&#60;`、一句话按样式边界碎成多个 run,到解析侧已无从还原。
/// 且**不会报错**。这正是引用回复失真的根因。
///
/// **走 `mget` 而不是单条 `messages/{id}`。** 两者带同样的 `raw_card_content` 参数,但对**复杂卡**
/// (带折叠面板/按钮的,summon agent 发的几乎都是)单条 get 只回「请升级至最新版本客户端」占位,
/// mget 却回完整 `<card>` 正文 —— 2026-08-12 用同一 bot 身份、同一条 message_id 实测对照。
///
/// 这个差别决定一条链路通不通:owner 引用 A 的卡片、@ B 处理时,B 走单条 get 拿到占位,
/// `looks_like_card_placeholder` 命中后落进 `ForeignUnreadable` 分支,只得到一句「自己去取」的提示,
/// **A 的正文一个字都进不了 B 的上下文**。旧注释断言「即便带 raw_card_content 也一样」——
/// 那个实测结论本身没错,错在把「这个接口读不到」推广成了「飞书读不到」。
pub fn fetch_message(base: &str, message_id: &str) -> String {
    format!(
        "{base}/open-apis/im/v1/messages/mget?card_msg_content_type=raw_card_content&user_id_type=open_id&with_sender_name=true&message_ids={message_id}"
    )
}

/// 原地更新一张已发的卡片(PATCH)。无必带参数。
pub fn update_card(base: &str, message_id: &str) -> String {
    format!("{base}/open-apis/im/v1/messages/{message_id}")
}

/// 撤回一条自己发的消息(DELETE)。与 [`update_card`] 同 URL,靠方法区分。
pub fn recall_message(base: &str, message_id: &str) -> String {
    format!("{base}/open-apis/im/v1/messages/{message_id}")
}

/// 创建飞书原生 CoT 消息。**必带** `receive_id_type=chat_id`。
pub fn cot_create(base: &str) -> String {
    format!("{base}/open-apis/im/v1/message_cot?receive_id_type=chat_id")
}

/// 向飞书原生 CoT 消息追加事件。创建与写事件共用资源路径，靠 HTTP 方法区分。
pub fn cot_events(base: &str) -> String {
    format!("{base}/open-apis/im/v1/message_cot")
}

/// 显式结束异常或容量已满的 CoT 载体。逻辑轮正常结束写 `RUN_FINISHED`。
pub fn cot_complete(base: &str, cot_id: &str, message_id: &str, reason: &str) -> String {
    format!(
        "{base}/open-apis/im/v1/message_cot/complete/{cot_id}?message_id={message_id}&reason={reason}"
    )
}

/// 按实际文档反查知识库中的原始创建者和当前所有者。
pub(crate) fn document_wiki_node(base: &str, token: &str, kind: &str) -> String {
    format!("{base}/open-apis/wiki/v2/spaces/get_node?token={token}&obj_type={kind}")
}

pub(crate) fn document_creator_search(base: &str) -> String {
    format!("{base}/open-apis/search/v2/doc_wiki/search")
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: &str = "https://open.feishu.cn";

    #[test]
    fn required_query_params_are_pinned() {
        assert!(document_metadata(B).contains("user_id_type=open_id"));
        assert!(
            document_comments(B, "doc_1", "docx").contains("file_type=docx&user_id_type=open_id")
        );
        let replies = document_replies(B, "doc_1", "sheet", "comment_1");
        assert!(replies.contains("/doc_1/comments/comment_1/replies?"));
        assert!(replies.contains("file_type=sheet&user_id_type=open_id"));
        assert!(replies.contains("page_size=100"));
        assert!(document_text(B, "doc_1").ends_with("/docx/v1/documents/doc_1/raw_content?lang=0"));
        // 漏了 id 类型:owner_id 按默认类型返回,应用所有者与发信人永远对不上。
        for (url, id_type) in [
            (application_info_open_id(B, "cli_1"), "user_id_type=open_id"),
            (
                application_info_union_id(B, "cli_1"),
                "user_id_type=union_id",
            ),
        ] {
            assert!(url.contains(id_type), "{url}");
            assert!(url.contains("lang=zh_cn"), "{url}");
            assert!(
                url.contains("/open-apis/application/v6/applications/cli_1?"),
                "{url}"
            );
        }
        // 漏了它:被引用的卡片被降级成扁平结构,样式/字面 `<>`/换行全失真,且飞书不报错。
        assert!(
            fetch_message(B, "om_1").contains("card_msg_content_type=raw_card_content"),
            "fetch_message 必须带 card_msg_content_type"
        );
        // 复杂卡(带折叠面板/按钮)只有 mget 回得出正文,单条 `messages/{id}` 恒回占位。
        // 2026-08-12 同身份同 message_id 实测对照;走错接口 = agent 间引用静默断链。
        assert!(
            fetch_message(B, "om_1").contains("/im/v1/messages/mget?"),
            "fetch_message 必须走 mget,单条 get 对复杂卡只回「请升级客户端」占位"
        );
        assert!(
            fetch_message(B, "om_1").contains("message_ids=om_1"),
            "mget 按 message_ids 查询参数传 id,不是路径段"
        );
        // nested sender 后续要按 open_id 查人名，不能依赖服务端默认值。
        assert!(
            fetch_message(B, "om_1").contains("user_id_type=open_id"),
            "fetch_message 必须把 nested sender 固定为 open_id"
        );
        // 漏了它:飞书按默认 id 类型解析 open_id,查不到人。
        assert!(
            user_info(B, "ou_1").contains("user_id_type=open_id"),
            "user_info 必须带 user_id_type"
        );
        // 漏了它:飞书不知道 receive_id 是 chat_id 还是 open_id。
        assert!(
            send_message(B, "chat_id").contains("receive_id_type=chat_id"),
            "send_message 必须带 receive_id_type"
        );
        assert!(
            cot_create(B).contains("receive_id_type=chat_id"),
            "cot_create 必须带 receive_id_type=chat_id"
        );
        // 漏了它:飞书不知道该按图片还是文件取资源。
        assert!(
            message_resource(B, "om_1", "f_1", "image").contains("type=image"),
            "message_resource 必须带 type"
        );
        // 漏了它:返回的 member_id 不是 open_id,对不上发信人。
        assert!(
            chat_members(B, "oc_1", None).contains("member_id_type=open_id"),
            "chat_members 必须带 member_id_type"
        );
        // 漏了它:on_ 串按 open_id 解析,查不到人。
        assert!(
            user_info_by_union(B, "on_1").contains("user_id_type=union_id"),
            "user_info_by_union 必须带 user_id_type=union_id"
        );
        // 漏了它:owner_id/user_id_list 按默认 id 类型解析,拉不进人且不报错。
        assert!(
            create_chat(B).contains("user_id_type=open_id"),
            "create_chat 必须带 user_id_type"
        );
        assert!(
            create_chat(B).contains("set_bot_manager=false"),
            "create_chat 必须钉死 set_bot_manager"
        );
    }

    /// 分页参数只在有 token 时出现:飞书对空 `page_token` 的容忍度不必去赌,首页就别带。
    #[test]
    fn chat_members_appends_page_token_only_when_present() {
        assert!(!chat_members(B, "oc_1", None).contains("page_token"));
        assert!(!chat_members(B, "oc_1", Some("")).contains("page_token"));
        assert!(chat_members(B, "oc_1", Some("tok_2")).ends_with("&page_token=tok_2"));
    }

    #[test]
    fn paths_are_wellformed() {
        assert_eq!(
            application_scopes(B),
            format!("{B}/open-apis/application/v6/scopes")
        );
        assert_eq!(bot_info(B), format!("{B}/open-apis/bot/v3/info"));
        assert_eq!(
            tenant_access_token(B),
            format!("{B}/open-apis/auth/v3/tenant_access_token/internal")
        );
        assert_eq!(
            reactions(B, "om_1"),
            format!("{B}/open-apis/im/v1/messages/om_1/reactions")
        );
        assert_eq!(
            reaction_delete(B, "om_1", "r_1"),
            format!("{B}/open-apis/im/v1/messages/om_1/reactions/r_1")
        );
        assert_eq!(
            chat_info(B, "oc_1"),
            format!("{B}/open-apis/im/v1/chats/oc_1")
        );
        assert_eq!(
            update_card(B, "om_1"),
            format!("{B}/open-apis/im/v1/messages/om_1")
        );
        assert_eq!(cot_events(B), format!("{B}/open-apis/im/v1/message_cot"));
        assert_eq!(
            cot_complete(B, "cot_1", "om_1", "error"),
            format!("{B}/open-apis/im/v1/message_cot/complete/cot_1?message_id=om_1&reason=error")
        );
    }

    /// 进 thread 的唯一途径。路径写错的后果是回复整个发不出去,不是发错地方。
    #[test]
    fn reply_message_targets_the_message_reply_endpoint() {
        assert_eq!(
            reply_message(B, "om_x"),
            format!("{B}/open-apis/im/v1/messages/om_x/reply")
        );
    }
}
