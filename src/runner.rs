#![allow(clippy::too_many_arguments)]

use crate::app;
use crate::cli::Cli;
use crate::event_reader;
use crate::explorer;
use crate::lua;
use crate::pipe_reader;
use crate::pwd_watcher;
use crate::ui;
use anyhow::Result;
use crossterm::event;
use crossterm::execute;
use crossterm::terminal as term;
use mlua::LuaSerdeExt;
use std::fs;
use std::io;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use tui::backend::CrosstermBackend;
use tui::Terminal;

pub fn get_tty() -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
}

fn call_lua(
    app: &app::App,
    lua: &mlua::Lua,
    func: &str,
    _silent: bool,
) -> Result<Option<Vec<app::ExternalMsg>>> {
    let _focus_index = app
        .directory_buffer()
        .map(|d| d.focus())
        .unwrap_or_default()
        .to_string();

    let arg = app.to_lua_arg();
    let arg = lua.to_value(&arg)?;
    lua::call(lua, func, arg)
}

fn call(app: &app::App, cmd: app::Command, silent: bool) -> io::Result<ExitStatus> {
    let focus_index = app
        .directory_buffer()
        .map(|d| d.focus())
        .unwrap_or_default()
        .to_string();

    let (stdin, stdout, stderr) = if silent {
        (Stdio::null(), Stdio::null(), Stdio::null())
    } else {
        (get_tty()?.into(), get_tty()?.into(), get_tty()?.into())
    };

    Command::new(cmd.command().clone())
        .env("XPLR_APP_VERSION", app.version())
        .env("XPLR_PID", &app.pid().to_string())
        .env("XPLR_INPUT_BUFFER", app.input_buffer().unwrap_or_default())
        .env("XPLR_FOCUS_PATH", app.focused_node_str())
        .env("XPLR_FOCUS_INDEX", focus_index)
        .env("XPLR_SESSION_PATH", app.session_path())
        .env("XPLR_PIPE_MSG_IN", app.pipe().msg_in())
        .env("XPLR_PIPE_SELECTION_OUT", app.pipe().selection_out())
        .env("XPLR_PIPE_HISTORY_OUT", app.pipe().history_out())
        .env("XPLR_MODE", app.mode_str())
        .env("XPLR_PIPE_RESULT_OUT", app.pipe().result_out())
        .env(
            "XPLR_PIPE_GLOBAL_HELP_MENU_OUT",
            app.pipe().global_help_menu_out(),
        )
        .env(
            "XPLR_PIPE_DIRECTORY_NODES_OUT",
            app.pipe().directory_nodes_out(),
        )
        .env("XPLR_PIPE_LOGS_OUT", app.pipe().logs_out())
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .args(cmd.args())
        .status()
}

fn start_fifo(path: &str, focus_path: &str) -> Result<fs::File> {
    match fs::OpenOptions::new().write(true).open(path) {
        Ok(mut file) => {
            writeln!(file, "{}", focus_path)?;
            Ok(file)
        }
        Err(e) => Err(e.into()),
    }
}

pub struct Runner {
    pwd: PathBuf,
    focused_path: Option<PathBuf>,
    config_file: Option<PathBuf>,
    extra_config_files: Vec<PathBuf>,
    on_load: Vec<app::ExternalMsg>,
    read_only: bool,
}

impl Runner {
    /// Create a new runner object passing the default arguments
    pub fn new() -> Result<Self> {
        Self::from_cli(Default::default())
    }

    /// Create a new runner object passing the given arguments
    pub fn from_cli(cli: Cli) -> Result<Self> {
        let basedir = std::env::current_dir()?;
        let mut pwd = cli
            .path
            .map(|p| if p.is_relative() { basedir.join(p) } else { p })
            .unwrap_or_else(|| basedir.clone());
        let mut focused_path = None;

        if pwd.is_file() {
            focused_path = pwd.file_name().map(|p| p.into());
            pwd = pwd.parent().map(|p| p.into()).unwrap_or(basedir);
        }

        Ok(Self {
            pwd,
            focused_path,
            config_file: cli.config,
            extra_config_files: cli.extra_config,
            on_load: cli.on_load,
            read_only: cli.read_only,
        })
    }

    /// Run the instance
    pub fn run(self) -> Result<Option<String>> {
        // Why unsafe? See https://github.com/sayanarijit/xplr/issues/309
        let lua = unsafe { mlua::Lua::unsafe_new() };
        let mut app = app::App::create(self.pwd, &lua, self.config_file, self.extra_config_files)?;
        app.config.general.set_read_only(self.read_only);

        fs::create_dir_all(app.session_path())?;

        let (tx_msg_in, rx_msg_in) = mpsc::channel();
        let (tx_event_reader, rx_event_reader) = mpsc::channel();
        let (tx_pwd_watcher, rx_pwd_watcher) = mpsc::channel();

        app = app.explore_pwd()?;

        app = if let Some(f) = self
            .focused_path
            .clone()
            .map(|f| f.to_string_lossy().to_string())
        {
            app.focus_by_file_name(&f, true)?
        } else {
            app.focus_first(true)?
        };

        explorer::explore_recursive_async(
            app.explorer_config().clone(),
            app.pwd().into(),
            self.focused_path,
            app.directory_buffer().map(|d| d.focus()).unwrap_or(0),
            tx_msg_in.clone(),
        );
        tx_pwd_watcher.send(app.pwd().clone())?;

        let mut result = Ok(None);
        let session_path = app.session_path().to_owned();

        term::enable_raw_mode()?;

        // Use a tty to enable piping results
        let mut stdout = get_tty()?;
        // let stdout = io::stdout();
        // let mut stdout = stdout.lock();
        execute!(stdout, term::EnterAlternateScreen)?;

        let mut fifo: Option<fs::File> = if let Some(path) = app.config().general().start_fifo() {
            match start_fifo(path, &app.focused_node_str()) {
                Ok(file) => Some(file),
                Err(e) => {
                    app = app.log_error(e.to_string())?;
                    None
                }
            }
        } else {
            None
        };

        let mut last_focus: Option<app::Node> = None;

        let mut mouse_enabled = app.config().general().enable_mouse();
        if mouse_enabled {
            if let Err(e) = execute!(stdout, event::EnableMouseCapture) {
                app = app.log_error(e.to_string())?;
            }
        }

        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.hide_cursor()?;

        // Threads
        event_reader::keep_reading(tx_msg_in.clone(), rx_event_reader);
        pwd_watcher::keep_watching(app.pwd(), tx_msg_in.clone(), rx_pwd_watcher)?;

        // Enqueue on_load messages
        for msg in self.on_load {
            tx_msg_in.send(app::Task::new(app::MsgIn::External(msg), None))?;
        }

        'outer: for task in rx_msg_in {
            match app.handle_task(task) {
                Ok(a) => {
                    app = a;
                    while let Some(msg) = app.pop_msg_out() {
                        use app::MsgOut::*;
                        match msg {
                            // NOTE: Do not schedule critical tasks via tx_msg_in in this loop.
                            // Try handling them immediately.
                            Enque(task) => {
                                tx_msg_in.send(task)?;
                            }

                            Quit => {
                                result = Ok(None);
                                break 'outer;
                            }

                            PrintPwdAndQuit => {
                                result = Ok(Some(format!("{}\n", app.pwd())));
                                break 'outer;
                            }

                            PrintFocusPathAndQuit => {
                                result = Ok(app
                                    .focused_node()
                                    .map(|n| format!("{}\n", n.absolute_path())));
                                break 'outer;
                            }

                            PrintSelectionAndQuit => {
                                result = Ok(Some(app.selection_str()));
                                break 'outer;
                            }

                            PrintResultAndQuit => {
                                result = Ok(Some(app.result_str()));
                                break 'outer;
                            }

                            PrintAppStateAndQuit => {
                                let out = serde_yaml::to_string(&app)?;
                                result = Ok(Some(out));
                                break 'outer;
                            }

                            Debug(path) => {
                                fs::write(&path, serde_yaml::to_string(&app)?)?;
                            }

                            ClearScreen => {
                                terminal.clear()?;
                            }

                            ExplorePwdAsync => {
                                explorer::explore_async(
                                    app.explorer_config().clone(),
                                    app.pwd().into(),
                                    app.focused_node().map(|n| n.relative_path().into()),
                                    app.directory_buffer().map(|d| d.focus()).unwrap_or(0),
                                    tx_msg_in.clone(),
                                );
                                tx_pwd_watcher.send(app.pwd().clone())?;
                            }

                            ExploreParentsAsync => {
                                explorer::explore_recursive_async(
                                    app.explorer_config().clone(),
                                    app.pwd().into(),
                                    app.focused_node().map(|n| n.relative_path().into()),
                                    app.directory_buffer().map(|d| d.focus()).unwrap_or(0),
                                    tx_msg_in.clone(),
                                );
                                tx_pwd_watcher.send(app.pwd().clone())?;
                            }

                            Refresh => {
                                // $PWD watcher
                                tx_pwd_watcher.send(app.pwd().clone())?;
                                // UI
                                terminal.draw(|f| ui::draw(f, &app, &lua))?;
                                // Fifo
                                let focus = app.focused_node();
                                if focus != last_focus.as_ref() {
                                    if let Some(ref mut file) = fifo {
                                        writeln!(file, "{}", app.focused_node_str())?;
                                    };
                                    last_focus = focus.cloned();
                                }
                            }

                            EnableMouse => {
                                if !mouse_enabled {
                                    match execute!(
                                        terminal.backend_mut(),
                                        event::EnableMouseCapture
                                    ) {
                                        Ok(_) => {
                                            mouse_enabled = true;
                                        }
                                        Err(e) => {
                                            app = app.log_error(e.to_string())?;
                                        }
                                    }
                                }
                            }

                            ToggleMouse => {
                                let msg = if mouse_enabled {
                                    app::ExternalMsg::DisableMouse
                                } else {
                                    app::ExternalMsg::EnableMouse
                                };
                                app = app
                                    .handle_task(app::Task::new(app::MsgIn::External(msg), None))?;
                            }

                            DisableMouse => {
                                if mouse_enabled {
                                    match execute!(
                                        terminal.backend_mut(),
                                        event::DisableMouseCapture
                                    ) {
                                        Ok(_) => {
                                            mouse_enabled = false;
                                        }
                                        Err(e) => {
                                            app = app.log_error(e.to_string())?;
                                        }
                                    }
                                }
                            }

                            StartFifo(path) => {
                                fifo = match start_fifo(&path, &app.focused_node_str()) {
                                    Ok(file) => Some(file),
                                    Err(e) => {
                                        app = app.log_error(e.to_string())?;
                                        None
                                    }
                                }
                            }

                            StopFifo => {
                                if let Some(file) = fifo {
                                    fifo = None;
                                    std::mem::drop(file);
                                }
                            }

                            ToggleFifo(path) => {
                                if let Some(file) = fifo {
                                    fifo = None;
                                    std::mem::drop(file);
                                } else {
                                    fifo = match start_fifo(&path, &app.focused_node_str()) {
                                        Ok(file) => Some(file),
                                        Err(e) => {
                                            app = app.log_error(e.to_string())?;
                                            None
                                        }
                                    }
                                }
                            }

                            CallLuaSilently(func) => {
                                tx_event_reader.send(true)?;

                                match call_lua(&app, &lua, &func, false) {
                                    Ok(Some(msgs)) => {
                                        for msg in msgs {
                                            app = app.handle_task(app::Task::new(
                                                app::MsgIn::External(msg),
                                                None,
                                            ))?;
                                        }
                                    }
                                    Ok(None) => {}
                                    Err(err) => {
                                        app = app.log_error(err.to_string())?;
                                    }
                                };

                                tx_event_reader.send(false)?;
                            }

                            CallSilently(cmd) => {
                                tx_event_reader.send(true)?;

                                app.write_pipes()?;
                                let status = call(&app, cmd, true)
                                    .map(|s| {
                                        if s.success() {
                                            Ok(())
                                        } else {
                                            Err(format!("process exited with code {}", &s))
                                        }
                                    })
                                    .unwrap_or_else(|e| Err(e.to_string()));

                                match pipe_reader::read_all(app.pipe().msg_in()) {
                                    Ok(msgs) => {
                                        for msg in msgs {
                                            app = app.handle_task(app::Task::new(
                                                app::MsgIn::External(msg),
                                                None,
                                            ))?;
                                        }
                                    }
                                    Err(err) => {
                                        app = app.log_error(err.to_string())?;
                                    }
                                };

                                app.cleanup_pipes()?;

                                if let Err(e) = status {
                                    app = app.log_error(e.to_string())?;
                                };

                                tx_event_reader.send(false)?;
                            }

                            CallLua(func) => {
                                execute!(terminal.backend_mut(), event::DisableMouseCapture)
                                    .unwrap_or_default();

                                tx_event_reader.send(true)?;

                                terminal.clear()?;
                                terminal.set_cursor(0, 0)?;
                                term::disable_raw_mode()?;
                                terminal.show_cursor()?;

                                match call_lua(&app, &lua, &func, false) {
                                    Ok(Some(msgs)) => {
                                        for msg in msgs {
                                            app = app.handle_task(app::Task::new(
                                                app::MsgIn::External(msg),
                                                None,
                                            ))?;
                                        }
                                    }
                                    Ok(None) => {}
                                    Err(err) => {
                                        app = app.log_error(err.to_string())?;
                                    }
                                };

                                terminal.clear()?;
                                term::enable_raw_mode()?;
                                terminal.hide_cursor()?;
                                tx_event_reader.send(false)?;

                                if mouse_enabled {
                                    match execute!(
                                        terminal.backend_mut(),
                                        event::EnableMouseCapture
                                    ) {
                                        Ok(_) => {
                                            mouse_enabled = true;
                                        }
                                        Err(e) => {
                                            app = app.log_error(e.to_string())?;
                                        }
                                    }
                                }
                            }

                            Call(cmd) => {
                                execute!(terminal.backend_mut(), event::DisableMouseCapture)
                                    .unwrap_or_default();

                                tx_event_reader.send(true)?;

                                terminal.clear()?;
                                terminal.set_cursor(0, 0)?;
                                term::disable_raw_mode()?;
                                terminal.show_cursor()?;

                                app.write_pipes()?;
                                let status = call(&app, cmd, false)
                                    .map(|s| {
                                        if s.success() {
                                            Ok(())
                                        } else {
                                            Err(format!("process exited with code {}", &s))
                                        }
                                    })
                                    .unwrap_or_else(|e| Err(e.to_string()));

                                match pipe_reader::read_all(app.pipe().msg_in()) {
                                    Ok(msgs) => {
                                        for msg in msgs {
                                            app = app.handle_task(app::Task::new(
                                                app::MsgIn::External(msg),
                                                None,
                                            ))?;
                                        }
                                    }
                                    Err(err) => {
                                        app = app.log_error(err.to_string())?;
                                    }
                                };

                                app.cleanup_pipes()?;

                                if let Err(e) = status {
                                    app = app.log_error(e.to_string())?;
                                };

                                terminal.clear()?;
                                term::enable_raw_mode()?;
                                terminal.hide_cursor()?;
                                tx_event_reader.send(false)?;

                                if mouse_enabled {
                                    match execute!(
                                        terminal.backend_mut(),
                                        event::EnableMouseCapture
                                    ) {
                                        Ok(_) => {
                                            mouse_enabled = true;
                                        }
                                        Err(e) => {
                                            app = app.log_error(e.to_string())?;
                                        }
                                    }
                                }
                            }
                        };
                    }
                }

                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }

        terminal.clear()?;
        terminal.set_cursor(0, 0)?;
        execute!(terminal.backend_mut(), term::LeaveAlternateScreen)?;
        execute!(terminal.backend_mut(), event::DisableMouseCapture).unwrap_or_default();
        term::disable_raw_mode()?;
        terminal.show_cursor()?;

        fs::remove_dir_all(session_path)?;

        result
    }
}
