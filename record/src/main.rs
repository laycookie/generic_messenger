use auth::AuthStore;
use iced::{window, Element, Task};
use pages::{
    chat::{Message as ChatMessage, MessangerWindow},
    Login, MyAppMessage, Page,
};

mod auth;
mod pages;

pub fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting");
    iced::daemon(App::title(), App::update, App::view)
        .run_with(|| {
            let app = App::default();
            let (_window_id, window_task) = window::open(window::Settings::default());

            (app, window_task.then(|_| Task::none()))
        })
        .inspect_err(|err| println!("{}", err))?;

    Ok(())
}

struct App {
    auth_store: AuthStore,
    memoryless_page: Box<dyn Page>,
}
impl Default for App {
    fn default() -> Self {
        let auth_store = AuthStore::new("./LoginInfo".into());

        let memoryless_page: Box<dyn Page>;
        if auth_store.is_empty() {
            memoryless_page = Box::new(Login::new());
        } else {
            let m = smol::block_on(async {
                MessangerWindow::new(auth_store.get_messangers().to_vec())
                    .await
                    .unwrap()
            });
            memoryless_page = Box::new(m);
        }

        Self {
            memoryless_page,
            auth_store,
        }
    }
}
impl App {
    fn title() -> &'static str {
        "record"
    }
    fn update(&mut self, message: MyAppMessage) -> impl Into<Task<MyAppMessage>> {
        match message {
            MyAppMessage::OpenChat(chat) => {
                self.auth_store.save_to_disk();
                self.memoryless_page = Box::new(chat);
                Task::none()
            }
            MyAppMessage::AddAuth(auth) => {
                self.auth_store.add_auth(auth.into());
                let m = MessangerWindow::new(self.auth_store.get_messangers().to_vec());
                Task::perform(async move { m.await.unwrap() }, |chat| {
                    MyAppMessage::OpenChat(chat)
                })
            }
            MyAppMessage::LoadConversation(msgs_store) => {
                let a = self.auth_store.get_messangers()[0].clone().auth;
                Task::perform(
                    async move {
                        let pq = a.param_query().unwrap();
                        pq.get_messanges(msgs_store, None).await.unwrap()
                    },
                    |f| MyAppMessage::Chat(ChatMessage::OpenConversation(f)),
                )
            }
            _ => self.memoryless_page.update(message),
        }
    }
    fn view(&self, _window: window::Id) -> Element<MyAppMessage> {
        self.memoryless_page.view()
    }
}
