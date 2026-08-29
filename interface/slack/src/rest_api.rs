use std::error::Error;

use facet::Facet;
use messenger_downloader::{Body as _, Fetch, Fresh};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};

use crate::{SLACK_API, SlackMessenger, api_types};

fn encode(value: &str) -> String {
    utf8_percent_encode(value, NON_ALPHANUMERIC).to_string()
}

impl SlackMessenger {
    async fn api_get<T: for<'a> Facet<'a>>(
        &self,
        method: &str,
        params: &[(&str, String)],
    ) -> Result<T, Box<dyn Error + Send + Sync>> {
        let query = params
            .iter()
            .map(|(key, value)| format!("{key}={}", encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        let url = if query.is_empty() {
            format!("{SLACK_API}/{method}")
        } else {
            format!("{SLACK_API}/{method}?{query}")
        };
        Fetch::<Fresh>::fetch(|| surf::get(&url), self.auth_header())
            .await?
            .json::<T>()
            .await
            .map_err(Into::into)
    }

    async fn api_post_json<T: for<'a> Facet<'a>>(
        &self,
        method: &str,
        body: Vec<u8>,
    ) -> Result<T, Box<dyn Error + Send + Sync>> {
        Fetch::<Fresh>::fetch(
            || {
                surf::post(format!("{SLACK_API}/{method}"))
                    .body(body.clone())
                    .content_type("application/json")
            },
            self.auth_header(),
        )
        .await?
        .json::<T>()
        .await
        .map_err(Into::into)
    }

    pub(crate) async fn rest_auth_test(
        &self,
    ) -> Result<api_types::AuthTestResponse, Box<dyn Error + Send + Sync>> {
        self.api_get::<api_types::AuthTestResponse>("auth.test", &[])
            .await?
            .into_result()
            .map_err(|err| format!("Slack auth.test failed: {err}").into())
    }

    pub(crate) async fn rest_user_info(
        &self,
        user_id: &str,
    ) -> Result<api_types::SlackUser, Box<dyn Error + Send + Sync>> {
        self.api_get::<api_types::UserInfoResponse>("users.info", &[("user", user_id.to_owned())])
            .await?
            .into_user()
            .map_err(|err| format!("Slack users.info failed for {user_id}: {err}").into())
    }

    pub(crate) async fn rest_users(
        &self,
    ) -> Result<Vec<api_types::SlackUser>, Box<dyn Error + Send + Sync>> {
        let mut cursor = None::<String>;
        let mut users = Vec::new();
        loop {
            let mut params = vec![("limit", "200".to_owned())];
            if let Some(cursor) = cursor.as_ref().filter(|cursor| !cursor.is_empty()) {
                params.push(("cursor", cursor.clone()));
            }

            let response = self
                .api_get::<api_types::UsersListResponse>("users.list", &params)
                .await?
                .into_result()
                .map_err(|err| format!("Slack users.list failed: {err}"))?;
            users.extend(response.members.unwrap_or_default());

            cursor = response
                .response_metadata
                .and_then(|metadata| metadata.next_cursor)
                .filter(|cursor| !cursor.is_empty());
            if cursor.is_none() {
                break;
            }
        }
        Ok(users)
    }

    pub(crate) async fn rest_conversations(
        &self,
        types: &str,
    ) -> Result<Vec<api_types::SlackConversation>, Box<dyn Error + Send + Sync>> {
        let mut cursor = None::<String>;
        let mut conversations = Vec::new();
        loop {
            let mut params = vec![
                ("types", types.to_owned()),
                ("exclude_archived", "true".to_owned()),
                ("limit", "200".to_owned()),
            ];
            if let Some(cursor) = cursor.as_ref().filter(|cursor| !cursor.is_empty()) {
                params.push(("cursor", cursor.clone()));
            }

            let response = self
                .api_get::<api_types::ConversationListResponse>("conversations.list", &params)
                .await?
                .into_result()
                .map_err(|err| format!("Slack conversations.list failed: {err}"))?;
            conversations.extend(response.channels.unwrap_or_default());

            cursor = response
                .response_metadata
                .and_then(|metadata| metadata.next_cursor)
                .filter(|cursor| !cursor.is_empty());
            if cursor.is_none() {
                break;
            }
        }
        Ok(conversations)
    }

    pub(crate) async fn rest_history(
        &self,
        channel_id: &str,
        before: Option<&str>,
    ) -> Result<Vec<api_types::SlackMessage>, Box<dyn Error + Send + Sync>> {
        let mut params = vec![
            ("channel", channel_id.to_owned()),
            ("limit", "50".to_owned()),
            ("inclusive", "false".to_owned()),
        ];
        if let Some(before) = before {
            params.push(("latest", before.to_owned()));
        }
        self.api_get::<api_types::HistoryResponse>("conversations.history", &params)
            .await?
            .into_messages()
            .map_err(|err| format!("Slack conversations.history failed: {err}").into())
    }

    pub(crate) async fn rest_send_message(
        &self,
        channel_id: &str,
        text: String,
    ) -> Result<(String, api_types::SlackMessage), Box<dyn Error + Send + Sync>> {
        let body = facet_json::to_vec(&api_types::PostMessageRequest {
            channel: channel_id.to_owned(),
            text,
        })?;
        self.api_post_json::<api_types::PostMessageResponse>("chat.postMessage", body)
            .await?
            .into_message()
            .map_err(|err| format!("Slack chat.postMessage failed: {err}").into())
    }
}
