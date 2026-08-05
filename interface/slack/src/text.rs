use std::error::Error;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::future::join_all;
use messenger_interface::{
    interface::{Ordering, Text},
    types::{Identifier, Message, Place, Revision, RichText, Room, User},
};

use crate::{SlackMessenger, api_types, query::user_identifier, stable_id};

fn slack_ts_time(ts: &str) -> Option<DateTime<Utc>> {
    let (secs, frac) = ts.split_once('.').unwrap_or((ts, ""));
    let secs = secs.parse::<i64>().ok()?;
    let micros = frac
        .chars()
        .take(6)
        .collect::<String>()
        .parse::<u32>()
        .unwrap_or(0);
    DateTime::from_timestamp(secs, micros * 1_000)
}

fn slack_ts_sort_key(ts: &str) -> (i64, u32) {
    let (secs, frac) = ts.split_once('.').unwrap_or((ts, ""));
    (
        secs.parse::<i64>().unwrap_or(0),
        frac.chars()
            .take(6)
            .collect::<String>()
            .parse::<u32>()
            .unwrap_or(0),
    )
}

fn bot_author(message: &api_types::SlackMessage) -> Option<Identifier<User>> {
    let bot_id = message.bot_id.as_ref()?;
    let name = message
        .username
        .as_ref()
        .filter(|name| !name.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| bot_id.clone());
    Some(Identifier::new(
        stable_id("bot", bot_id),
        User {
            name,
            tag: Some(bot_id.clone()),
            icon: None,
            status: None,
            voice: None,
        },
    ))
}

impl SlackMessenger {
    async fn message_author(&self, message: &api_types::SlackMessage) -> Option<Identifier<User>> {
        if let Some(user_id) = message.user.as_deref() {
            return match self.user_info_cached(user_id).await {
                Ok(user) => Some(user_identifier(user)),
                Err(_) => Some(Identifier::new(
                    stable_id("user", user_id),
                    User {
                        name: user_id.to_owned(),
                        tag: Some(user_id.to_owned()),
                        icon: None,
                        status: None,
                        voice: None,
                    },
                )),
            };
        }
        bot_author(message)
    }

    async fn interface_message(
        &self,
        channel_id: &str,
        message: api_types::SlackMessage,
    ) -> Identifier<Message> {
        let id = self.remember_message(channel_id, &message.ts);
        let author = self.message_author(&message).await;
        let created_at = slack_ts_time(&message.ts);
        let edit_at = message
            .edited
            .as_ref()
            .and_then(|edited| edited.ts.as_deref().and_then(slack_ts_time).or(created_at));
        let content = RichText::plain(message.text.unwrap_or_default());

        let (content, history) = match edit_at {
            Some(edit_at) if Some(edit_at) != created_at => (
                Revision {
                    at: Some(edit_at),
                    text: content,
                },
                vec![Revision {
                    at: created_at,
                    text: RichText::default(),
                }],
            ),
            _ => (
                Revision {
                    at: created_at,
                    text: content,
                },
                Vec::new(),
            ),
        };

        Identifier::new(
            id,
            Message {
                content,
                history,
                reactions: Vec::new(),
                author,
            },
        )
    }
}

#[async_trait]
impl Text for SlackMessenger {
    async fn get_messages(
        &self,
        location: &Identifier<Place<Room>>,
        load_messages_before: Option<Identifier<Message>>,
        ordering: Ordering,
    ) -> Result<Vec<Identifier<Message>>, Box<dyn Error + Sync + Send>> {
        let channel_id = self.channel_id(*location.id())?;
        let before = match load_messages_before {
            Some(message) => Some(self.message_ts(*message.id())?),
            None => None,
        };
        let mut messages = self
            .rest_history(&channel_id, before.as_deref())
            .await?
            .into_iter()
            .filter(|message| message.message_type.as_deref().unwrap_or("message") == "message")
            .filter(|message| message.text.as_deref().is_some_and(|text| !text.is_empty()))
            .collect::<Vec<_>>();

        if ordering == Ordering::Time {
            messages.sort_by_key(|message| slack_ts_sort_key(&message.ts));
        }

        Ok(join_all(
            messages
                .into_iter()
                .map(|message| self.interface_message(&channel_id, message)),
        )
        .await)
    }

    async fn send_message(
        &self,
        location: &Identifier<Place<Room>>,
        contents: Message,
    ) -> Result<Identifier<Message>, Box<dyn Error + Sync + Send>> {
        let channel_id = self.channel_id(*location.id())?;
        let text = contents.content.text.to_plain();
        let (channel_id, message) = self.rest_send_message(&channel_id, text).await?;
        Ok(self.interface_message(&channel_id, message).await)
    }
}
