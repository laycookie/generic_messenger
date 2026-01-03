use iced::advanced::graphics::core::Element;
use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{self, Tree, Widget, tree};
use iced::advanced::{Clipboard, Shell, renderer};
use iced::{Color, Event, Length, Point, Rectangle, Size, mouse, touch};
pub struct Divider<'a, Message>
where
    Message: Clone,
{
    width: f32,
    height: f32,
    on_change: Box<dyn Fn(f32) -> Message + 'a>,
    on_release: Option<Message>,
}
impl<'a, Message> Divider<'a, Message>
where
    Message: Clone,
{
    //TODO: Somehow make the divider be a part of the widgets you want to resize
    pub fn new<F>(width: f32, height: f32, on_change: F) -> Self
    where
        F: 'a + Fn(f32) -> Message,
    {
        Self {
            width,
            height,
            on_change: Box::new(on_change),
            on_release: None,
        }
    }
    pub fn on_release(mut self, on_release: Message) -> Self {
        self.on_release = Some(on_release);
        self
    }
}

impl<'a, Message, Theme, Renderer> Widget<Message, Theme, Renderer> for Divider<'_, Message>
where
    Message: Clone,
    Renderer: renderer::Renderer,
{
    fn size(&self) -> Size<Length> {
        Size {
            width: Length::from(self.width),
            height: Length::from(self.height),
        }
    }

    fn layout(
        &mut self,
        _tree: &mut widget::Tree,
        _renderer: &Renderer,
        _limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(Size::new(self.width, self.height))
    }

    fn draw(
        &self,
        _state: &widget::Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        renderer.fill_quad(
            renderer::Quad {
                bounds: layout.bounds(),

                ..renderer::Quad::default()
            },
            Color::from_rgba(0.5, 0.5, 0.5, 0.5),
        );
    }
    fn state(&self) -> tree::State {
        tree::State::new(State::new())
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let foo = tree.state.downcast_mut::<State>();
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
            | Event::Touch(touch::Event::FingerPressed { .. }) => {
                if let Some(cursor_position) = cursor.position_over(layout.bounds()) {
                    foo.is_dragging = true;
                    foo.prev = cursor_position;
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
            | Event::Touch(touch::Event::FingerLifted { .. } | touch::Event::FingerLost { .. }) => {
                if foo.is_dragging {
                    if let Some(on_release) = self.on_release.clone() {
                        shell.publish(on_release);
                    }
                    foo.is_dragging = false;
                }
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                if foo.is_dragging {
                    let delta = position.x - foo.prev.x;
                    let divider_bound = layout.bounds().x;
                    if (delta < 0.0 && position.x > divider_bound)
                        || (delta > 0.0 && position.x < divider_bound)
                    {
                        foo.prev = *position;
                        return;
                    }
                    shell.publish((self.on_change)(delta));
                }
                foo.prev = *position;
            }
            _ => (),
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<State>();

        if state.is_dragging || cursor.is_over(layout.bounds()) {
            mouse::Interaction::ResizingHorizontally
        } else {
            mouse::Interaction::default()
        }
    }
}

impl<'a, Message, Theme, Renderer> From<Divider<'a, Message>>
    for Element<'a, Message, Theme, Renderer>
where
    Renderer: 'a + renderer::Renderer,
    Message: 'a + Clone,
    Theme: 'a,
{
    fn from(divider: Divider<'a, Message>) -> Self {
        Self::new(divider)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct State {
    is_dragging: bool,
    prev: Point,
}
impl State {
    /// Creates a new [`State`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}
