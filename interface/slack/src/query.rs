use std::error::Error;

use async_trait::async_trait;
use futures::future::join_all;
use messenger_interface::{
    interface::Query,
    types::{Details, House, Identifier, Place, Room, RoomCapabilities, User},
};

use crate::{SlackMessenger, api_types, stable_id};

fn first_nonempty<'a>(values: impl IntoIterator<Item = Option<&'a String>>) -> Option<String> {
    values
        .into_iter()
        .flatten()
        .map(|value| value.trim())
        .find(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) fn user_name(user: &api_types::SlackUser) -> String {
    first_nonempty([
        user.profile
            .as_ref()
            .and_then(|profile| profile.display_name.as_ref()),
        user.profile
            .as_ref()
            .and_then(|profile| profile.real_name.as_ref()),
        user.real_name.as_ref(),
        user.name.as_ref(),
    ])
    .unwrap_or_else(|| user.id.clone())
}

pub(crate) fn user_identifier(user: api_types::SlackUser) -> Identifier<User> {
    Identifier::new(
        stable_id("user", &user.id),
        User {
            name: user_name(&user),
            tag: user.name.clone(),
            icon: None,
            status: None,
            voice: None,
        },
    )
}

fn conversation_sort_key(conversation: &api_types::SlackConversation) -> (u8, u64, String) {
    let kind = if conversation.is_im.unwrap_or(false) {
        0
    } else if conversation.is_mpim.unwrap_or(false) {
        1
    } else {
        2
    };
    let recent = conversation.updated.unwrap_or(0);
    let name = conversation
        .name_normalized
        .as_ref()
        .or(conversation.name.as_ref())
        .cloned()
        .unwrap_or_else(|| conversation.id.clone())
        .to_lowercase();
    (kind, u64::MAX - recent, name)
}

impl SlackMessenger {
    async fn conversation_room(
        &self,
        conversation: api_types::SlackConversation,
    ) -> Identifier<Place<Room>> {
        let id = self.remember_channel(&conversation.id);
        let mut members = None;
        let mut name = conversation
            .name
            .clone()
            .or(conversation.name_normalized.clone())
            .unwrap_or_else(|| conversation.id.clone());

        if conversation.is_im.unwrap_or(false)
            && let Some(user_id) = conversation.user.as_deref()
            && !conversation.is_user_deleted.unwrap_or(false)
            && let Ok(user) = self.user_info_cached(user_id).await
        {
            let member = user_identifier(user);
            name = member.name.clone();
            members = Some(vec![member]);
        }

        let room = Room::new(RoomCapabilities::Text, members, None, None);
        Identifier::new(id, Place::new(name, None, room))
    }
}

#[async_trait]
impl Query for SlackMessenger {
    async fn client_user(&self) -> Result<Identifier<User>, Box<dyn Error + Sync + Send>> {
        let auth = self.auth_test().await?;
        let user_id = auth
            .user_id
            .as_deref()
            .ok_or("Slack auth.test did not return a user_id")?;
        Ok(user_identifier(self.user_info_cached(user_id).await?))
    }

    async fn contacts(&self) -> Result<Vec<Identifier<User>>, Box<dyn Error + Sync + Send>> {
        let users = self.rest_users().await?;
        self.cache_users(&users);
        let mut contacts = users
            .into_iter()
            .filter(|user| !user.deleted.unwrap_or(false))
            .filter(|user| !user.is_bot.unwrap_or(false))
            .filter(|user| !user.is_app_user.unwrap_or(false))
            .map(user_identifier)
            .collect::<Vec<_>>();
        contacts.sort_by_cached_key(|contact| contact.name.to_lowercase());
        Ok(contacts)
    }

    async fn rooms(&self) -> Result<Vec<Identifier<Place<Room>>>, Box<dyn Error + Sync + Send>> {
        let mut conversations = self.rest_conversations("im,mpim").await?;
        conversations.retain(|conversation| !conversation.is_archived.unwrap_or(false));
        conversations.sort_by_key(conversation_sort_key);
        Ok(join_all(
            conversations
                .into_iter()
                .map(|conversation| self.conversation_room(conversation)),
        )
        .await)
    }

    async fn houses(&self) -> Result<Vec<Identifier<Place<House>>>, Box<dyn Error + Sync + Send>> {
        let auth = self.auth_test().await?;
        let team_id = auth
            .team_id
            .as_deref()
            .ok_or("Slack auth.test did not return a team_id")?;
        let name = auth.team.unwrap_or_else(|| "Slack workspace".to_owned());
        Ok(vec![Identifier::new(
            stable_id("team", team_id),
            Place::new(name, None, House::new(None)),
        )])
    }

    async fn house_details(
        &self,
        _house: Identifier<Place<House>>,
    ) -> Result<House, Box<dyn Error + Sync + Send>> {
        let mut conversations = self
            .rest_conversations("public_channel,private_channel")
            .await?;
        conversations.retain(|conversation| !conversation.is_archived.unwrap_or(false));
        conversations.sort_by_key(conversation_sort_key);
        let rooms = conversations
            .into_iter()
            .map(|conversation| {
                let id = self.remember_channel(&conversation.id);
                let name = conversation
                    .name
                    .or(conversation.name_normalized)
                    .unwrap_or_else(|| conversation.id.clone());
                Identifier::new(
                    id,
                    Place::new(
                        name,
                        None,
                        Room::new(RoomCapabilities::Text, None, None, None),
                    ),
                )
            })
            .collect();
        Ok(House::new(Some(rooms)))
    }

    async fn inspect_user(
        &self,
        user: &Identifier<User>,
    ) -> Result<Details, Box<dyn Error + Sync + Send>> {
        let mut details = Details::default();
        details.push("Name", user.name.clone());
        details.push("User ID", user.id().to_string());
        if let Some(tag) = &user.tag {
            details.push("Handle", tag.clone());
        }
        Ok(details)
    }

    async fn inspect_house(
        &self,
        house: &Identifier<Place<House>>,
    ) -> Result<Details, Box<dyn Error + Sync + Send>> {
        let auth = self.auth_test().await?;
        let mut details = Details::default();
        details.push("Workspace", house.name.clone());
        details.push_opt("Team ID", auth.team_id);
        details.push_opt("Authed user", auth.user);
        Ok(details)
    }
}
