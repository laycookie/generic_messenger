use std::borrow::Borrow;

use adaptors::types::{Chan, Identifier};
use iced::{
    Alignment, Element, Length, Padding, Task,
    advanced::{self, renderer},
    widget::{
        Button, Column, Scrollable, button, column, container, image, row,
        scrollable::{self, Direction, Scrollbar},
        text,
    },
};

use crate::{
    messanger_unifier::Messangers,
    pages::messenger::{Main, PLACEHOLDER_PFP, chat::Chat, contacts::Contacts},
};

#[derive(Debug)]
pub struct Sidebar {
    pub width: f32,
}

// enum Message {
//     LoadConversation {
//         handle: crate::messanger_unifier::MessangerHandle,
//         conversation: Identifier<Chan>,
//     },
// }

#[derive(Debug, Clone)]
pub enum Action {
    OpenContacts,
    OpenChat {
        handle: crate::messanger_unifier::MessangerHandle,
        conversation: Identifier<Chan>,
    },
}

impl Sidebar {
    pub fn new(width: f32) -> Self {
        Self { width }
    }

    pub fn get_element<'a, Theme, Renderer>(
        &self,
        messengers: &'a Messangers,
    ) -> Element<'a, Action, Theme, Renderer>
    where
        Renderer: 'a + renderer::Renderer + advanced::image::Renderer + advanced::text::Renderer,
        <Renderer as advanced::image::Renderer>::Handle:
            for<'c> From<&'c std::path::PathBuf> + From<&'static str>,
        Theme: 'a + scrollable::Catalog + button::Catalog + container::Catalog + text::Catalog,
    {
        Scrollable::new(
            column![
                Button::new(
                    container("Contacts")
                        .width(Length::Fill)
                        .align_x(Alignment::Center)
                )
                .on_press(Action::OpenContacts)
                .width(Length::Fill),
                messengers
                    .data_iter()
                    .zip(messengers.interface_iter())
                    .flat_map(|(data, (m_handle, _))| {
                        data.conversations.iter().map(|conversation| {
                            Button::new({
                                let image = match &conversation.icon {
                                    Some(icon) => image(icon),
                                    None => image(PLACEHOLDER_PFP),
                                };
                                row![
                                    container(image.height(Length::Fixed(28.0)))
                                        .padding(Padding::new(0.0).right(10.0)),
                                    conversation.name.as_str()
                                ]
                            })
                            .width(Length::Fill)
                            .on_press(Action::OpenChat {
                                handle: *m_handle,
                                conversation: conversation.to_owned(),
                            })
                        })
                    })
                    .fold(Column::new(), |column, widget| column.push(widget))
            ]
            .width(self.width),
        )
        .direction(Direction::Vertical(
            Scrollbar::default().width(7).scroller_width(7),
        ))
        .into()
    }
}
