use iced::{
    Background, Color, Element, Length, border,
    widget::{Space, container},
};
use messenger_interface::types::{Identifier, Place, Room, User, UserStatus};
use messenger_unifier::MessengerData;

const SIZE: f32 = 8.0;

fn same_user(left: &Identifier<User>, right: &Identifier<User>) -> bool {
    left.id() == right.id()
        || left
            .tag
            .as_ref()
            .zip(right.tag.as_ref())
            .is_some_and(|(left, right)| left == right)
}

pub fn status_indicator<'a, Message: 'a>(
    status: Option<UserStatus>,
) -> Option<Element<'a, Message>> {
    let color = match status? {
        UserStatus::Online => Color::from_rgb(0.20, 0.70, 0.32),
        UserStatus::Idle => Color::from_rgb(0.86, 0.62, 0.12),
        UserStatus::DoNotDisturb => Color::from_rgb(0.82, 0.20, 0.20),
        UserStatus::Offline => Color::from_rgb(0.50, 0.50, 0.50),
    };

    Some(
        container(Space::new())
            .width(Length::Fixed(SIZE))
            .height(Length::Fixed(SIZE))
            .style(move |_theme| container::Style {
                background: Some(Background::Color(color)),
                border: border::rounded(SIZE / 2.0),
                ..Default::default()
            })
            .into(),
    )
}

pub fn known_user_status(
    data: Option<&MessengerData>,
    user: &Identifier<User>,
) -> Option<UserStatus> {
    data.and_then(|data| {
        data.contacts
            .iter()
            .find(|contact| same_user(contact, user))
            .and_then(|contact| contact.status)
            .or_else(|| {
                data.profile
                    .as_ref()
                    .filter(|profile| same_user(profile, user))
                    .and_then(|profile| profile.status)
            })
    })
    .or(user.status)
}

/// Status of the partner in a 1:1 conversation, resolved from the room's
/// sole member. Voice `participants` are irrelevant here: membership is
/// what identifies the conversation, and it survives calls starting and
/// ending.
pub fn one_to_one_room_status(
    data: &MessengerData,
    room: &Identifier<Place<Room>>,
) -> Option<UserStatus> {
    let [partner] = room.members.as_deref()? else {
        return None;
    };
    known_user_status(Some(data), partner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use messenger_interface::types::RoomCapabilities;
    use messenger_unifier::MessengerData;

    fn data() -> MessengerData {
        MessengerData::default()
    }

    fn user(
        id: u64,
        name: &str,
        tag: Option<&str>,
        status: Option<UserStatus>,
    ) -> Identifier<User> {
        Identifier::new(
            id,
            User {
                name: name.to_owned(),
                tag: tag.map(str::to_owned),
                icon: None,
                status,
                voice: None,
            },
        )
    }

    #[test]
    fn contact_status_can_match_participant_by_tag() {
        let mut data = data();
        data.contacts
            .push(user(1, "Alice", Some("alice"), Some(UserStatus::Online)));
        let participant = user(2, "Alice", Some("alice"), None);

        assert_eq!(
            known_user_status(Some(&data), &participant),
            Some(UserStatus::Online)
        );
    }

    #[test]
    fn group_dm_has_no_single_user_status() {
        let mut data = data();
        data.contacts
            .push(user(1, "Alice", Some("alice"), Some(UserStatus::Online)));
        let room = Identifier::new(
            10,
            Place::new(
                "group".to_owned(),
                None,
                Room::new(
                    RoomCapabilities::Text,
                    Some(vec![
                        user(1, "Alice", Some("alice"), None),
                        user(2, "Bob", Some("bob"), Some(UserStatus::Idle)),
                    ]),
                    None,
                    None,
                ),
            ),
        );

        assert_eq!(one_to_one_room_status(&data, &room), None);
    }

    /// The DM partner's status comes from room *membership*; the voice
    /// roster coming and going (calls starting/ending) must not affect it.
    #[test]
    fn dm_status_survives_voice_roster_changes() {
        let mut data = data();
        data.contacts
            .push(user(1, "Alice", Some("alice"), Some(UserStatus::Online)));
        let room = Identifier::new(
            10,
            Place::new(
                "Alice".to_owned(),
                None,
                Room::new(
                    RoomCapabilities::Text | RoomCapabilities::Voice,
                    Some(vec![user(1, "Alice", Some("alice"), None)]),
                    // Emptied voice roster, as after a call ended.
                    Some(Vec::new()),
                    None,
                ),
            ),
        );

        assert_eq!(
            one_to_one_room_status(&data, &room),
            Some(UserStatus::Online)
        );
    }
}
