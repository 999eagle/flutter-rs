use crate::{
    channel::Channel,
    ffi::{
        FlutterEngine,
        FlutterPointerPhase,
        FlutterPointerSignalKind,
        FlutterPointerMouseButtons,
    },
    plugins::PluginRegistrar,
    utils::WindowUnwrap,
};

use log::{debug, info};
use std::{
    collections::{
        VecDeque,
        HashMap,
    },
    sync::Mutex,
    sync::{
        mpsc,
        mpsc::{Receiver, Sender},
        Arc,
    },
};
use tokio::prelude::Future;
use tokio::runtime::{Runtime, TaskExecutor};

use lazy_static::lazy_static;

const SCROLL_SPEED: f64 = 50.0; // seems to be about 2.5 lines of text
#[cfg(not(target_os = "macos"))]
const BY_WORD_MODIFIER_KEY: glfw::Modifiers = glfw::Modifiers::Control;
#[cfg(target_os = "macos")]
const BY_WORD_MODIFIER_KEY: glfw::Modifiers = glfw::Modifiers::Alt;
const SELECT_MODIFIER_KEY: glfw::Modifiers = glfw::Modifiers::Shift;
#[cfg(not(target_os = "macos"))]
const FUNCTION_MODIFIER_KEY: glfw::Modifiers = glfw::Modifiers::Control;
#[cfg(target_os = "macos")]
const FUNCTION_MODIFIER_KEY: glfw::Modifiers = glfw::Modifiers::Super;

pub(crate) type MainThreadWindowFn = Box<FnMut(&mut glfw::Window) + Send>;
pub(crate) type MainThreadChannelFn = (&'static str, Box<FnMut(&Channel) + Send>);
pub(crate) type MainThreadPlatformMsg = (String, Vec<u8>);
pub(crate) type MainTheadWindowStateFn = Box<dyn FnMut(&mut DesktopWindowState) + Send>;

pub(crate) enum MainThreadCallback {
    WindowFn(MainThreadWindowFn),
    ChannelFn(MainThreadChannelFn),
    PlatformMessage(MainThreadPlatformMsg),
    WindowStateFn(MainTheadWindowStateFn),
}

pub struct DesktopWindowState {
    window_ref: *mut glfw::Window,
    pub window_event_receiver: Receiver<(f64, glfw::WindowEvent)>,
    pub runtime: Runtime,
    main_thread_receiver: Receiver<MainThreadCallback>,
    pub init_data: Arc<InitData>,
    pointer_currently_added: bool,
    window_pixels_per_screen_coordinate: f64,
    isolate_created: bool,
    defered_events: VecDeque<glfw::WindowEvent>,
    pub plugin_registrar: PluginRegistrar,
}

/// Data accessible during initialization and on the main thread.
pub struct InitData {
    pub engine: Arc<FlutterEngine>,
    pub runtime_data: Arc<RuntimeData>,
}

/// Data accessible during runtime. Implements Send to be used in message handling.
#[derive(Clone)]
pub struct RuntimeData {
    pub(crate) main_thread_sender: Sender<MainThreadCallback>,
    pub task_executor: TaskExecutor,
}

impl RuntimeData {
    pub fn with_window_result<F, R>(&self, mut f: F) -> Result<R, crate::error::RuntimeMessageError>
    where
        F: FnMut(&mut glfw::Window) -> R + Send + 'static,
        R: Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        self.main_thread_sender
            .send(MainThreadCallback::WindowFn(Box::new(move |window| {
                let result = f(window);
                tx.send(result).unwrap();
            })))?;
        Ok(rx.recv()?)
    }

    pub fn with_window<F>(&self, mut f: F) -> Result<(), crate::error::RuntimeMessageError>
    where
        F: FnMut(&mut glfw::Window) + Send + 'static,
    {
        self.main_thread_sender
            .send(MainThreadCallback::WindowFn(Box::new(move |window| {
                f(window);
            })))?;
        Ok(())
    }

    pub fn with_channel<F>(
        &self,
        channel_name: &'static str,
        mut f: F,
    ) -> Result<(), crate::error::RuntimeMessageError>
    where
        F: FnMut(&Channel) + Send + 'static,
    {
        self.main_thread_sender
            .send(MainThreadCallback::ChannelFn((
                channel_name,
                Box::new(move |channel| {
                    f(channel);
                }),
            )))?;
        Ok(())
    }

    pub fn send_message(
        &self,
        channel_name: String,
        data: Vec<u8>,
    ) -> Result<(), crate::error::RuntimeMessageError> {
        self.main_thread_sender
            .send(MainThreadCallback::PlatformMessage((channel_name, data)))?;
        Ok(())
    }

    pub fn with_window_state<F>(&self, f: F) -> Result<(), crate::error::RuntimeMessageError>
    where
        F: FnMut(&mut DesktopWindowState) + Send + 'static,
    {
        self.main_thread_sender
            .send(MainThreadCallback::WindowStateFn(Box::new(f)))?;
        Ok(())
    }
}

impl DesktopWindowState {
    pub fn window(&mut self) -> &mut glfw::Window {
        self.window_ref.window()
    }

    pub fn new(
        window_ref: *mut glfw::Window,
        window_event_receiver: Receiver<(f64, glfw::WindowEvent)>,
        engine: FlutterEngine,
    ) -> Self {
        let runtime = Runtime::new().unwrap();
        let (main_tx, main_rx) = mpsc::channel();
        let runtime_data = Arc::new(RuntimeData {
            main_thread_sender: main_tx,
            task_executor: runtime.executor(),
        });
        let engine = Arc::new(engine);
        let init_data = Arc::new(InitData {
            engine: engine.clone(),
            runtime_data,
        });

        // register window and engine globally
        unsafe {
            use glfw::Context;
            let window: &glfw::Window = &*window_ref;
            let mut guard = ENGINES.lock().unwrap();
            guard.insert(WindowSafe(window.window_ptr()), FlutterEngineSafe(engine));
        }

        Self {
            window_ref,
            window_event_receiver,
            runtime,
            main_thread_receiver: main_rx,
            pointer_currently_added: false,
            window_pixels_per_screen_coordinate: 0.0,
            plugin_registrar: PluginRegistrar::new(Arc::downgrade(&init_data)),
            isolate_created: false,
            defered_events: VecDeque::new(),
            init_data,
        }
    }

    pub fn send_scale_or_size_change(&mut self) {
        let window = self.window();
        let window_size = window.get_size();
        let framebuffer_size = window.get_framebuffer_size();
        let scale = window.get_content_scale();
        self.window_pixels_per_screen_coordinate =
            f64::from(framebuffer_size.0) / f64::from(window_size.0);
        debug!(
            "Setting framebuffer size to {:?}, scale to {}",
            framebuffer_size, scale.0
        );
        self.init_data.engine.send_window_metrics_event(
            framebuffer_size.0,
            framebuffer_size.1,
            f64::from(scale.0),
        );
    }

    fn send_pointer_event(
        &mut self,
        phase: FlutterPointerPhase,
        x: f64,
        y: f64,
        signal_kind: FlutterPointerSignalKind,
        scroll_delta_x: f64,
        scroll_delta_y: f64,
        buttons: FlutterPointerMouseButtons,
    ) {
        if !self.pointer_currently_added
            && phase != FlutterPointerPhase::Add
            && phase != FlutterPointerPhase::Remove {
                self.send_pointer_event(
                    FlutterPointerPhase::Add,
                    x,
                    y,
                    FlutterPointerSignalKind::None,
                    0.0,
                    0.0,
                    buttons,
                );
            }
        if self.pointer_currently_added && phase == FlutterPointerPhase::Add
            || !self.pointer_currently_added && phase == FlutterPointerPhase::Remove {
                return;
            }
        self.init_data.engine.send_pointer_event(
            phase,
            x * self.window_pixels_per_screen_coordinate,
            y * self.window_pixels_per_screen_coordinate,
            signal_kind,
            scroll_delta_x * self.window_pixels_per_screen_coordinate,
            scroll_delta_y * self.window_pixels_per_screen_coordinate,
            buttons,
        );

        match phase {
            FlutterPointerPhase::Add => self.pointer_currently_added = true,
            FlutterPointerPhase::Remove => self.pointer_currently_added = false,
            _ => {}
        }
    }

    pub fn handle_glfw_event(&mut self, event: glfw::WindowEvent) {
        if !self.isolate_created {
            self.defered_events.push_back(event);
            return;
        }

        match event {
            glfw::WindowEvent::CursorEnter(entered) => {
                let cursor_pos = self.window().get_cursor_pos();
                self.send_pointer_event(
                    if entered {
                        FlutterPointerPhase::Add
                    } else {
                        FlutterPointerPhase::Remove
                    },
                    cursor_pos.0,
                    cursor_pos.1,
                    FlutterPointerSignalKind::None,
                    0.0,
                    0.0,
                    FlutterPointerMouseButtons::Primary,
                );
            }
            glfw::WindowEvent::CursorPos(x, y) => {
                // fix error when dragging cursor out of a window
                if !self.pointer_currently_added {
                    return;
                }
                let phase = if self.window().get_mouse_button(glfw::MouseButtonLeft)
                    == glfw::Action::Press
                {
                    FlutterPointerPhase::Move
                } else {
                    FlutterPointerPhase::Hover
                };
                self.send_pointer_event(phase, x, y, FlutterPointerSignalKind::None, 0.0, 0.0, FlutterPointerMouseButtons::Primary);
            }
            glfw::WindowEvent::MouseButton(
                glfw::MouseButton::Button4,
                glfw::Action::Press,
                _modifiers,
            ) => {
                self.plugin_registrar.with_plugin(
                    |navigation: &crate::plugins::NavigationPlugin| {
                        navigation.pop_route();
                    },
                );
            }
            glfw::WindowEvent::MouseButton(buttons, action, _modifiers) => {
                // fix error when keeping primary button down
                // and alt+tab away from the window and release
                if !self.pointer_currently_added {
                    return;
                }
                let (x, y) = self.window().get_cursor_pos();
                let phase = if action == glfw::Action::Press {
                    FlutterPointerPhase::Down
                } else {
                    FlutterPointerPhase::Up
                };
                let buttons = match buttons {
                    glfw::MouseButtonLeft => FlutterPointerMouseButtons::Primary,
                    glfw::MouseButtonRight => FlutterPointerMouseButtons::Secondary,
                    glfw::MouseButtonMiddle => FlutterPointerMouseButtons::Middle,
                    glfw::MouseButton::Button4 => FlutterPointerMouseButtons::Back,
                    glfw::MouseButton::Button5 => FlutterPointerMouseButtons::Forward,
                    _ => FlutterPointerMouseButtons::Primary,
                };
                self.send_pointer_event(phase, x, y, FlutterPointerSignalKind::None, 0.0, 0.0, buttons);
            }
            glfw::WindowEvent::Scroll(scroll_delta_x, scroll_delta_y) => {
                let (x, y) = self.window().get_cursor_pos();
                let phase = if self.window().get_mouse_button(glfw::MouseButtonLeft)
                    == glfw::Action::Press
                {
                    FlutterPointerPhase::Move
                } else {
                    FlutterPointerPhase::Hover
                };
                self.send_pointer_event(
                    phase,
                    x,
                    y,
                    FlutterPointerSignalKind::Scroll,
                    scroll_delta_x * SCROLL_SPEED,
                    -scroll_delta_y * SCROLL_SPEED,
                    FlutterPointerMouseButtons::Primary,
                );
            }
            glfw::WindowEvent::FramebufferSize(_, _) => {
                self.send_scale_or_size_change();
            }
            glfw::WindowEvent::ContentScale(_, _) => {
                self.send_scale_or_size_change();
            }
            glfw::WindowEvent::Char(char) => self.plugin_registrar.with_plugin_mut(
                |text_input: &mut crate::plugins::TextInputPlugin| {
                    text_input.with_state(|state| {
                        state.add_characters(&char.to_string());
                    });
                    text_input.notify_changes();
                },
            ),
            glfw::WindowEvent::Key(key, scancode, glfw::Action::Press, modifiers)
            | glfw::WindowEvent::Key(key, scancode, glfw::Action::Repeat, modifiers) => {
                // TODO: move this to TextInputPlugin
                match key {
                    glfw::Key::Enter => self.plugin_registrar.with_plugin_mut(
                        |text_input: &mut crate::plugins::TextInputPlugin| {
                            text_input.with_state(|state| {
                                state.add_characters(&"\n");
                            });
                            text_input.notify_changes();
                        },
                    ),
                    glfw::Key::Up => self.plugin_registrar.with_plugin_mut(
                        |text_input: &mut crate::plugins::TextInputPlugin| {
                            text_input.with_state(|state| {
                                state.move_up(modifiers.contains(SELECT_MODIFIER_KEY));
                            });
                            text_input.notify_changes();
                        },
                    ),
                    glfw::Key::Down => self.plugin_registrar.with_plugin_mut(
                        |text_input: &mut crate::plugins::TextInputPlugin| {
                            text_input.with_state(|state| {
                                state.move_down(modifiers.contains(SELECT_MODIFIER_KEY));
                            });
                            text_input.notify_changes();
                        },
                    ),
                    glfw::Key::Backspace => self.plugin_registrar.with_plugin_mut(
                        |text_input: &mut crate::plugins::TextInputPlugin| {
                            text_input.with_state(|state| {
                                state.backspace();
                            });
                            text_input.notify_changes();
                        },
                    ),
                    glfw::Key::Delete => self.plugin_registrar.with_plugin_mut(
                        |text_input: &mut crate::plugins::TextInputPlugin| {
                            text_input.with_state(|state| {
                                state.delete();
                            });
                            text_input.notify_changes();
                        },
                    ),
                    glfw::Key::Left => self.plugin_registrar.with_plugin_mut(
                        |text_input: &mut crate::plugins::TextInputPlugin| {
                            text_input.with_state(|state| {
                                state.move_left(
                                    modifiers.contains(BY_WORD_MODIFIER_KEY),
                                    modifiers.contains(SELECT_MODIFIER_KEY),
                                );
                            });
                            text_input.notify_changes();
                        },
                    ),
                    glfw::Key::Right => self.plugin_registrar.with_plugin_mut(
                        |text_input: &mut crate::plugins::TextInputPlugin| {
                            text_input.with_state(|state| {
                                state.move_right(
                                    modifiers.contains(BY_WORD_MODIFIER_KEY),
                                    modifiers.contains(SELECT_MODIFIER_KEY),
                                );
                            });
                            text_input.notify_changes();
                        },
                    ),
                    glfw::Key::Home => self.plugin_registrar.with_plugin_mut(
                        |text_input: &mut crate::plugins::TextInputPlugin| {
                            text_input.with_state(|state| {
                                state.move_to_beginning(modifiers.contains(SELECT_MODIFIER_KEY));
                            });
                            text_input.notify_changes();
                        },
                    ),
                    glfw::Key::End => self.plugin_registrar.with_plugin_mut(
                        |text_input: &mut crate::plugins::TextInputPlugin| {
                            text_input.with_state(|state| {
                                state.move_to_end(modifiers.contains(SELECT_MODIFIER_KEY));
                            });
                            text_input.notify_changes();
                        },
                    ),
                    glfw::Key::A => {
                        if modifiers.contains(FUNCTION_MODIFIER_KEY) {
                            self.plugin_registrar.with_plugin_mut(
                                |text_input: &mut crate::plugins::TextInputPlugin| {
                                    text_input.with_state(|state| {
                                        state.select_all();
                                    });
                                    text_input.notify_changes();
                                },
                            )
                        }
                    }
                    glfw::Key::X => {
                        if modifiers.contains(FUNCTION_MODIFIER_KEY) {
                            let window = self.window_ref.window();
                            self.plugin_registrar.with_plugin_mut(
                                |text_input: &mut crate::plugins::TextInputPlugin| {
                                    text_input.with_state(|state| {
                                        window.set_clipboard_string(state.get_selected_text());
                                        state.delete_selected();
                                    });
                                    text_input.notify_changes();
                                },
                            )
                        }
                    }
                    glfw::Key::C => {
                        if modifiers.contains(FUNCTION_MODIFIER_KEY) {
                            let window = self.window_ref.window();
                            self.plugin_registrar.with_plugin_mut(
                                |text_input: &mut crate::plugins::TextInputPlugin| {
                                    text_input.with_state(|state| {
                                        window.set_clipboard_string(state.get_selected_text());
                                    });
                                    text_input.notify_changes();
                                },
                            )
                        }
                    }
                    glfw::Key::V => {
                        if modifiers.contains(FUNCTION_MODIFIER_KEY) {
                            let window = self.window_ref.window();
                            self.plugin_registrar.with_plugin_mut(
                                |text_input: &mut crate::plugins::TextInputPlugin| {
                                    text_input.with_state(|state| {
                                        if let Some(text) = window.get_clipboard_string() {
                                            state.add_characters(&text);
                                        } else {
                                            info!("Tried to paste non-text data");
                                        }
                                    });
                                    text_input.notify_changes();
                                },
                            )
                        }
                    }
                    _ => {}
                }

                self.plugin_registrar.with_plugin_mut(
                    |keyevent: &mut crate::plugins::KeyEventPlugin| {
                        keyevent.key_action(true, key, scancode, modifiers);
                    },
                );
            },
            glfw::WindowEvent::Key(key, scancode, glfw::Action::Release, modifiers) => {
                self.plugin_registrar.with_plugin_mut(
                    |keyevent: &mut crate::plugins::KeyEventPlugin| {
                        keyevent.key_action(false, key, scancode, modifiers);
                    },
                );
            },
            _ => {}
        }
    }

    pub fn handle_main_thread_callbacks(&mut self) {
        let callbacks: Vec<MainThreadCallback> = self.main_thread_receiver.try_iter().collect();
        for cb in callbacks {
            match cb {
                MainThreadCallback::WindowFn(mut f) => f(self.window_ref.window()),
                MainThreadCallback::ChannelFn((name, mut f)) => {
                    self.plugin_registrar
                        .channel_registry
                        .with_channel(name, |channel| {
                            f(channel);
                        });
                }
                MainThreadCallback::PlatformMessage(msg) => {
                    let platform_msg = crate::ffi::PlatformMessage {
                        channel: msg.0.into(),
                        message: &msg.1,
                        response_handle: None,
                    };
                    self.init_data.engine.send_platform_message(platform_msg);
                }
                MainThreadCallback::WindowStateFn(mut f) => f(self),
            }
        }
    }

    pub fn with_window_and_plugin_mut_result<F, P, R>(&mut self, f: F) -> Option<R>
    where
        F: FnOnce(&mut glfw::Window, &mut P) -> R,
        P: crate::plugins::Plugin + 'static,
    {
        let window = self.window_ref.window();
        let mut result = None;
        self.plugin_registrar.with_plugin_mut(|p: &mut P| {
            result = Some(f(window, p));
        });

        result
    }

    pub fn set_isolate_created(&mut self) {
        self.isolate_created = true;

        while self.defered_events.len() > 0 {
            let evt = self.defered_events.pop_front().unwrap();
            self.handle_glfw_event(evt);
        }
    }

    pub fn shutdown(self) {
        let mut guard = ENGINES.lock().unwrap();
        unsafe {
            use glfw::Context;
            let window: &glfw::Window = &*self.window_ref;
            guard.remove(&WindowSafe(window.window_ptr()));
        }

        self.init_data.engine.shutdown();
        self.runtime.shutdown_now().wait().unwrap();
    }
}

/// Wrap glfw::Window, so that it could be used in a lazy_static HashMap
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct WindowSafe(*mut glfw::ffi::GLFWwindow);

unsafe impl Send for WindowSafe {}
unsafe impl Sync for WindowSafe {}

struct FlutterEngineSafe(Arc<FlutterEngine>);

unsafe impl Send for FlutterEngineSafe {}
unsafe impl Sync for FlutterEngineSafe {}

// This HashMap is usded to look up FlutterEngine using glfw Window
lazy_static! {
    static ref ENGINES: Mutex<HashMap<WindowSafe, FlutterEngineSafe>> = Mutex::new(HashMap::new());
}

pub fn get_engine(window: *mut glfw::ffi::GLFWwindow) -> Option<Arc<FlutterEngine>> {
    let guard = ENGINES.lock().unwrap();
    guard.get(&WindowSafe(window)).map(|v| v.0.clone())
}
