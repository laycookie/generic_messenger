use std::mem;

use auth::AuthStore;
use futures::channel::mpsc::Sender;
use iced::{window, Element, Subscription, Task};
use pages::{chat::MessangerWindow, Login, MyAppMessage};
use socket::{ReciverEvent, SocketConnection};

mod auth;
mod pages;
mod socket;

pub fn main() -> Result<(), Box<dyn std::error::Error>> {
    iced::daemon(App::title(), App::update, App::view)
        .subscription(App::subscription)
        .run_with(|| {
            let app = App::default();
            let (_window_id, window_task) = window::open(window::Settings::default());

            // Dirty hack, fix later
            if mem::discriminant(&app.page) == mem::discriminant(&Page::Todo) {
                let messangers = app.auth_store.get_auths();
                (
                    app,
                    window_task.then(|_| Task::none()).chain(Task::perform(
                        async move { MessangerWindow::new(messangers).await.unwrap() },
                        |m| MyAppMessage::OpenPage(Page::Chat(m)),
                    )),
                )
            } else {
                (app, window_task.then(|_| Task::none()))
            }
        })
        .inspect_err(|err| println!("{}", err))?;

    Ok(())
}

#[derive(Debug)]
pub enum Page {
    Login(Login),
    Chat(MessangerWindow),
    Todo,
}

struct App {
    socket: Option<Sender<ReciverEvent>>,
    auth_store: AuthStore,
    page: Page,
}

impl Default for App {
    fn default() -> Self {
        let auth_store = AuthStore::new("./LoginInfo".into());

        if auth_store.is_empty() {
            Self {
                socket: None,
                auth_store,
                page: Page::Login(Login::new()),
            }
        } else {
            // Part of the dirty hack, fix later
            Self {
                socket: None,
                auth_store,
                page: Page::Todo,
            }
        }
    }
}

impl App {
    fn title() -> &'static str {
        "record"
    }
    fn update(&mut self, message: MyAppMessage) -> impl Into<Task<MyAppMessage>> {
        match message {
            // Global Actions
            MyAppMessage::OpenPage(page) => {
                self.page = page;
                Task::none()
            }
            MyAppMessage::AuthDiskSync => {
                self.auth_store.save_to_disk();
                Task::none()
            }
            MyAppMessage::SocketEvent(event) => match event {
                socket::SocketEvent::Connect(mut socket_connection) => {
                    println!("Socket connected");
                    let auths = self.auth_store.get_auths();
                    auths.iter().for_each(|auth| {
                        let _temp =
                            socket_connection.try_send(ReciverEvent::Connection(auth.clone()));
                    });
                    self.socket = Some(socket_connection);
                    Task::none()
                }
                socket::SocketEvent::Message(t) => {
                    println!("Rec: {:?}", t);
                    Task::none()
                }
            },
            // Pages
            MyAppMessage::Login(message) => {
                let Page::Login(login) = &mut self.page else {
                    return Task::none();
                };
                match login.update(message) {
                    pages::login::Action::None => Task::none(),
                    pages::login::Action::Run(task) => task.map(MyAppMessage::Login),
                    pages::login::Action::Login(messenger) => {
                        self.auth_store.add_auth(messenger);

                        let messangers = self.auth_store.get_auths();
                        Task::perform(async { MessangerWindow::new(messangers).await }, |chat| {
                            match chat {
                                Ok(chat) => MyAppMessage::OpenPage(Page::Chat(chat)),
                                Err(_) => todo!(),
                            }
                        })
                        .chain(Task::done(MyAppMessage::AuthDiskSync))
                    }
                }
            }
            MyAppMessage::Chat(message) => {
                let Page::Chat(chat) = &mut self.page else {
                    return Task::none();
                };
                match chat.update(message, &self.auth_store) {
                    pages::chat::Action::None => Task::none(),
                    pages::chat::Action::Run(task) => task.map(MyAppMessage::Chat),
                }
            }
        }
    }
    fn view(&self, _window: window::Id) -> Element<MyAppMessage> {
        match &self.page {
            Page::Login(login) => login.view().map(MyAppMessage::Login),
            Page::Chat(chat) => chat.view().map(MyAppMessage::Chat),
            Page::Todo => iced::widget::text("Todo").into(),
        }
    }
    fn subscription(&self) -> Subscription<MyAppMessage> {
        Subscription::run(SocketConnection::new).map(|t| MyAppMessage::SocketEvent(t))
    }
}
