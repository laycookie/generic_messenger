use facet::Facet;

#[derive(Facet, Clone)]
pub(crate) struct AuthTestResponse {
    pub ok: bool,
    pub error: Option<String>,
    pub team: Option<String>,
    pub team_id: Option<String>,
    pub user: Option<String>,
    pub user_id: Option<String>,
}

impl AuthTestResponse {
    pub(crate) fn into_result(self) -> Result<Self, String> {
        if self.ok {
            Ok(self)
        } else {
            Err(self.error.unwrap_or_else(|| "unknown_error".to_owned()))
        }
    }
}

#[derive(Facet, Clone, Default)]
pub(crate) struct ResponseMetadata {
    pub next_cursor: Option<String>,
}

#[derive(Facet, Clone)]
pub(crate) struct ConversationListResponse {
    pub ok: bool,
    pub error: Option<String>,
    pub channels: Option<Vec<SlackConversation>>,
    pub response_metadata: Option<ResponseMetadata>,
}

impl ConversationListResponse {
    pub(crate) fn into_result(self) -> Result<Self, String> {
        if self.ok {
            Ok(self)
        } else {
            Err(self.error.unwrap_or_else(|| "unknown_error".to_owned()))
        }
    }
}

#[derive(Facet, Clone)]
pub(crate) struct SlackConversation {
    pub id: String,
    pub name: Option<String>,
    pub name_normalized: Option<String>,
    pub is_im: Option<bool>,
    pub is_mpim: Option<bool>,
    pub is_archived: Option<bool>,
    pub is_user_deleted: Option<bool>,
    pub user: Option<String>,
    pub num_members: Option<u32>,
    pub priority: Option<f64>,
    pub updated: Option<u64>,
}

#[derive(Facet, Clone)]
pub(crate) struct UsersListResponse {
    pub ok: bool,
    pub error: Option<String>,
    pub members: Option<Vec<SlackUser>>,
    pub response_metadata: Option<ResponseMetadata>,
}

impl UsersListResponse {
    pub(crate) fn into_result(self) -> Result<Self, String> {
        if self.ok {
            Ok(self)
        } else {
            Err(self.error.unwrap_or_else(|| "unknown_error".to_owned()))
        }
    }
}

#[derive(Facet, Clone)]
pub(crate) struct UserInfoResponse {
    pub ok: bool,
    pub error: Option<String>,
    pub user: Option<SlackUser>,
}

impl UserInfoResponse {
    pub(crate) fn into_user(self) -> Result<SlackUser, String> {
        if self.ok {
            self.user.ok_or_else(|| "missing_user".to_owned())
        } else {
            Err(self.error.unwrap_or_else(|| "unknown_error".to_owned()))
        }
    }
}

#[derive(Facet, Clone)]
pub(crate) struct SlackUser {
    pub id: String,
    pub name: Option<String>,
    pub real_name: Option<String>,
    pub deleted: Option<bool>,
    pub is_bot: Option<bool>,
    pub is_app_user: Option<bool>,
    pub profile: Option<SlackUserProfile>,
}

#[derive(Facet, Clone)]
pub(crate) struct SlackUserProfile {
    pub display_name: Option<String>,
    pub real_name: Option<String>,
}

#[derive(Facet, Clone)]
pub(crate) struct HistoryResponse {
    pub ok: bool,
    pub error: Option<String>,
    pub messages: Option<Vec<SlackMessage>>,
    pub has_more: Option<bool>,
}

impl HistoryResponse {
    pub(crate) fn into_messages(self) -> Result<Vec<SlackMessage>, String> {
        if self.ok {
            Ok(self.messages.unwrap_or_default())
        } else {
            Err(self.error.unwrap_or_else(|| "unknown_error".to_owned()))
        }
    }
}

#[derive(Facet, Clone)]
pub(crate) struct SlackMessage {
    #[facet(rename = "type")]
    pub message_type: Option<String>,
    pub subtype: Option<String>,
    pub user: Option<String>,
    pub bot_id: Option<String>,
    pub username: Option<String>,
    pub text: Option<String>,
    pub ts: String,
    pub edited: Option<SlackMessageEdited>,
}

#[derive(Facet, Clone)]
pub(crate) struct SlackMessageEdited {
    pub ts: Option<String>,
}

#[derive(Facet)]
pub(crate) struct PostMessageRequest {
    pub channel: String,
    pub text: String,
}

#[derive(Facet, Clone)]
pub(crate) struct PostMessageResponse {
    pub ok: bool,
    pub error: Option<String>,
    pub channel: Option<String>,
    pub ts: Option<String>,
    pub message: Option<SlackMessage>,
}

impl PostMessageResponse {
    pub(crate) fn into_message(self) -> Result<(String, SlackMessage), String> {
        if !self.ok {
            return Err(self.error.unwrap_or_else(|| "unknown_error".to_owned()));
        }
        let channel = self.channel.ok_or_else(|| "missing_channel".to_owned())?;
        let message = self.message.ok_or_else(|| "missing_message".to_owned())?;
        Ok((channel, message))
    }
}
