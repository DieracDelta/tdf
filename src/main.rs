use core::{
	error::Error,
	num::{NonZeroU32, NonZeroUsize}
};
use std::{
	borrow::Cow,
	ffi::OsString,
	io::{BufReader, Read as _, Stdout, Write as _, stdout},
	mem,
	path::PathBuf,
	process::Command,
	sync::{Arc, Mutex},
	time::Duration
};

use crossterm::{
	event::{Event as CrosstermEvent, EventStream, poll, read},
	execute,
	terminal::{
		EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
		enable_raw_mode, window_size
	}
};
use debounce::EventDebouncer;
use flexi_logger::FileSpec;
use flume::{Sender, r#async::RecvStream};
use futures_util::{FutureExt as _, stream::StreamExt as _};
use kittage::{
	action::Action,
	delete::{ClearOrDelete, DeleteConfig, WhichToDelete},
	error::{TerminalError, TransmitError}
};
use notify::{Event, EventKind, RecursiveMode, Watcher as _};
use ratatui::{Terminal, backend::CrosstermBackend};
use ratatui_image::{
	FontSize,
	picker::{Picker, ProtocolType}
};
use tdf::{
	PrerenderLimit,
	converter::{ConvertedPage, ConverterMsg, run_conversion_loop},
	kitty::{
		DisplayErr, KittyDisplay, KittyPlacementState, TmuxAnchor, display_kitty_images,
		do_shms_work, run_action
	},
	renderer::{self, MUPDF_BLACK, MUPDF_WHITE, RenderError, RenderInfo, RenderNotif},
	tui::{BottomMessage, InputAction, MessageSetting, Tui}
};

// Dummy struct for easy errors in main
struct WrappedErr(Cow<'static, str>);

impl std::fmt::Display for WrappedErr {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

impl std::fmt::Debug for WrappedErr {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		std::fmt::Display::fmt(self, f)
	}
}

impl std::error::Error for WrappedErr {}

fn reset_term() {
	_ = disable_raw_mode();
	_ = execute!(
		std::io::stdout(),
		LeaveAlternateScreen,
		crossterm::cursor::Show,
		crossterm::event::DisableMouseCapture,
		crossterm::event::DisableFocusChange
	);
}

fn drain_pending_input_events() {
	for _ in 0..256 {
		let Ok(has_event) = poll(Duration::from_millis(0)) else {
			break;
		};
		if !has_event {
			break;
		}
		let _ = read();
	}
}

fn query_tmux_anchor() -> Result<TmuxAnchor, WrappedErr> {
	let tmux_pane = std::env::var("TMUX_PANE")
		.map_err(|_| WrappedErr("TMUX_PANE is not set while running inside tmux".into()))?;
	let query_with_format = |fmt: &str| -> Result<std::process::Output, WrappedErr> {
		Command::new("tmux")
			.args(["display-message", "-p", "-t", &tmux_pane, fmt])
			.output()
			.map_err(|e| WrappedErr(format!("Couldn't query tmux pane offsets: {e}").into()))
	};

	let output = query_with_format(
		"#{window_offset_x} #{window_offset_y} #{pane_left} #{pane_top} #{pane_width} #{pane_height}"
	)?;

	if !output.status.success() {
		let stderr = String::from_utf8_lossy(&output.stderr);
		return Err(WrappedErr(
			format!("tmux pane offset query failed: {}", stderr.trim()).into()
		));
	}

	let parse_tokens = |tokens: &[&str]| -> Result<TmuxAnchor, WrappedErr> {
		match tokens {
			// Newer tmux fields including window offsets.
			[wx, wy, pl, pt, pw, ph, ..] => {
				let window_offset_x = wx.parse::<u16>().map_err(|e| {
					WrappedErr(format!("Couldn't parse tmux window_offset_x: {e}").into())
				})?;
				let window_offset_y = wy.parse::<u16>().map_err(|e| {
					WrappedErr(format!("Couldn't parse tmux window_offset_y: {e}").into())
				})?;
				let pane_left = pl
					.parse::<u16>()
					.map_err(|e| WrappedErr(format!("Couldn't parse tmux pane_left: {e}").into()))?;
				let pane_top = pt
					.parse::<u16>()
					.map_err(|e| WrappedErr(format!("Couldn't parse tmux pane_top: {e}").into()))?;
				let pane_width = pw.parse::<u16>().map_err(|e| {
					WrappedErr(format!("Couldn't parse tmux pane_width: {e}").into())
				})?;
				let pane_height = ph.parse::<u16>().map_err(|e| {
					WrappedErr(format!("Couldn't parse tmux pane_height: {e}").into())
				})?;
				Ok(TmuxAnchor {
					window_offset_x,
					window_offset_y,
					pane_left,
					pane_top,
					pane_width,
					pane_height
				})
			}
			// Fallback for older tmux output: pane offsets only.
			[pl, pt, pw, ph, ..] => {
				let pane_left = pl
					.parse::<u16>()
					.map_err(|e| WrappedErr(format!("Couldn't parse tmux pane_left: {e}").into()))?;
				let pane_top = pt
					.parse::<u16>()
					.map_err(|e| WrappedErr(format!("Couldn't parse tmux pane_top: {e}").into()))?;
				let pane_width = pw.parse::<u16>().map_err(|e| {
					WrappedErr(format!("Couldn't parse tmux pane_width: {e}").into())
				})?;
				let pane_height = ph.parse::<u16>().map_err(|e| {
					WrappedErr(format!("Couldn't parse tmux pane_height: {e}").into())
				})?;
				Ok(TmuxAnchor {
					window_offset_x: 0,
					window_offset_y: 0,
					pane_left,
					pane_top,
					pane_width,
					pane_height
				})
			}
			_ => Err(WrappedErr("tmux pane offset query returned too few fields".into()))
		}
	};

	let stdout = String::from_utf8_lossy(&output.stdout);
	let tokens = stdout.split_whitespace().collect::<Vec<_>>();
	let anchor = if tokens.len() >= 6 {
		parse_tokens(&tokens)?
	} else {
		let fallback = query_with_format("#{pane_left} #{pane_top} #{pane_width} #{pane_height}")?;
		if !fallback.status.success() {
			let stderr = String::from_utf8_lossy(&fallback.stderr);
			return Err(WrappedErr(
				format!("tmux pane offset fallback query failed: {}", stderr.trim()).into()
			));
		}
		let fb_stdout = String::from_utf8_lossy(&fallback.stdout);
		let fb_tokens = fb_stdout.split_whitespace().collect::<Vec<_>>();
		parse_tokens(&fb_tokens)?
	};

	Ok(anchor)
}

fn query_tmux_pane_active() -> Result<bool, WrappedErr> {
	let tmux_pane = std::env::var("TMUX_PANE")
		.map_err(|_| WrappedErr("TMUX_PANE is not set while running inside tmux".into()))?;
	let output = Command::new("tmux")
		.args(["display-message", "-p", "-t", &tmux_pane, "#{pane_active}"])
		.output()
		.map_err(|e| WrappedErr(format!("Couldn't query tmux pane active state: {e}").into()))?;

	if !output.status.success() {
		let stderr = String::from_utf8_lossy(&output.stderr);
		return Err(WrappedErr(
			format!("tmux pane active query failed: {}", stderr.trim()).into()
		));
	}

	let stdout = String::from_utf8_lossy(&output.stdout);
	let val = stdout.trim();
	Ok(val == "1")
}

#[tokio::main]
async fn main() -> Result<(), WrappedErr> {
	let result = inner_main().await;
	reset_term();
	result
}

async fn inner_main() -> Result<(), WrappedErr> {
	let hook = std::panic::take_hook();
	std::panic::set_hook(Box::new(move |info| {
		reset_term();
		hook(info);
	}));

	#[cfg(feature = "tracing")]
	console_subscriber::init();

	const DEFAULT_DEBOUNCE_DELAY: Duration = Duration::from_millis(50);

	let flags = xflags::parse_or_exit! {
		/// Display the pdf with the pages starting at the right hand size and moving left and
		/// adjust input keys to match
		optional -r,--r-to-l
		/// The maximum number of pages to display together, horizontally, at a time
		optional -m,--max-wide max_wide: NonZeroUsize
		/// Fullscreen the pdf (hide document name, page count, etc)
		optional -f,--fullscreen
		/// The time to wait for the file to stop changing before reloading, in milliseconds.
		/// Defaults to 50ms.
		optional --reload-delay reload_delay: u64
		/// The number of pages to prerender surrounding the currently-shown page; 0 means no
		/// limit. By default, there is no limit.
		optional -p,--prerender prerender: usize
		/// Custom white color, specified in css format (e.g. "FFFFFF" or "rgb(255, 255, 255)")
		optional -w,--white-color white: String
		/// Custom black color, specified in css format (e.g "000000" or "rgb(0, 0, 0)")
		optional -b,--black-color black: String
		/// Print the version and exit
		optional --version
		/// PDF file to read
		optional file: PathBuf
	};

	if flags.version {
		println!("{}", env!("CARGO_PKG_VERSION"));
		return Ok(());
	}

	let Some(file) = flags.file else {
		return Err(WrappedErr(
			"Please specify the file to open, e.g. `tdf ./my_example_pdf.pdf`".into()
		));
	};

	let path = file
		.canonicalize()
		.map_err(|e| WrappedErr(format!("Cannot canonicalize provided file: {e}").into()))?;

	let black = flags
		.black_color
		.as_deref()
		.map(|color| {
			parse_color_to_i32(color).map_err(|e| {
				WrappedErr(
					format!(
						"Couldn't parse black color {color:?}: {e} - is it formatted like a CSS color?"
					)
					.into()
				)
			})
		})
		.transpose()?
		.unwrap_or(MUPDF_BLACK);

	let white = flags
		.white_color
		.as_deref()
		.map(|color| {
			parse_color_to_i32(color).map_err(|e| {
				WrappedErr(
					format!(
						"Couldn't parse white color {color:?}: {e} - is it formatted like a CSS color?"
					)
					.into()
				)
			})
		})
		.transpose()?
		.unwrap_or(MUPDF_WHITE);

	// need to keep it around throughout the lifetime of the program, but don't rly need to use it.
	// Just need to make sure it doesn't get dropped yet.
	let maybe_logger = if std::env::var("RUST_LOG").is_ok() {
		Some(
			flexi_logger::Logger::try_with_env()
				.map_err(|e| WrappedErr(format!("Couldn't create initial logger: {e}").into()))?
				.log_to_file(FileSpec::try_from("./debug.log").map_err(|e| {
					WrappedErr(format!("Couldn't create FileSpec for logger: {e}").into())
				})?)
				.start()
				.map_err(|e| WrappedErr(format!("Can't start logger: {e}").into()))?
		)
	} else {
		None
	};

	let (watch_to_render_tx, render_rx) = flume::unbounded();
	let to_renderer = watch_to_render_tx.clone();

	let (render_tx, tui_rx) = flume::unbounded();
	let watch_to_tui_tx = render_tx.clone();

	let mut watcher = notify::recommended_watcher(on_notify_ev(
		watch_to_tui_tx,
		watch_to_render_tx,
		path.file_name()
			.ok_or_else(|| WrappedErr("Path does not have a last component??".into()))?
			.to_owned(),
		flags
			.reload_delay
			.map_or(DEFAULT_DEBOUNCE_DELAY, Duration::from_millis)
	))
	.map_err(|e| WrappedErr(format!("Couldn't start watching the provided file: {e}").into()))?;

	// So we have to watch the parent directory of the file that we are interested in because the
	// `notify` library works on inodes, and if the file is deleted, that inode is gone as well,
	// and then the notify library just gives up on trying to watch for the file reappearing. Imo
	// they should start watching the parent directory if the file is deleted, and then wait for it
	// to reappear and then begin watching it again, but whatever. It seems they've made their
	// opinion on this clear
	// (https://github.com/notify-rs/notify/issues/113#issuecomment-281836995) so whatever, guess
	// we have to do this annoying workaround.
	watcher
		.watch(
			path.parent().expect("The root directory is not a PDF"),
			RecursiveMode::NonRecursive
		)
		.map_err(|e| WrappedErr(format!("Can't watch the provided file: {e}").into()))?;

	let mut window_size = window_size().map_err(|e| {
		WrappedErr(format!("Can't get your current terminal window size: {e}").into())
	})?;

	if window_size.width == 0 || window_size.height == 0 {
		let (w, h) = get_font_size_through_stdio()?;

		window_size.width = w;
		window_size.height = h;
	}

	let cell_height_px = window_size.height / window_size.rows;
	let cell_width_px = window_size.width / window_size.columns;

	execute!(
		std::io::stdout(),
		EnterAlternateScreen,
		crossterm::cursor::Hide,
		crossterm::event::EnableMouseCapture,
		crossterm::event::EnableFocusChange
	)
	.map_err(|e| {
		WrappedErr(
			format!(
				"Couldn't enter the alternate screen and hide the cursor for proper presentation: {e}"
			)
			.into()
		)
	})?;

	// We need to create `picker` on this thread because if we create it on the `renderer` thread,
	// it messes up something with user input. Input never makes it to the crossterm thing
	let picker = Picker::from_query_stdio()
		.or_else(|e| match e {
			ratatui_image::errors::Errors::NoFontSize if
				window_size.width != 0
				&& window_size.height != 0
				&& window_size.columns != 0
				&& window_size.rows != 0 =>
			{
				// the 'equivalent' that is suggested instead is not the same. We need to keep
				// calling this.
				#[expect(deprecated)]
				Ok(Picker::from_fontsize((cell_width_px, cell_height_px)))
			},
			ratatui_image::errors::Errors::NoFontSize => Err(WrappedErr(
				"Unable to detect your terminal's font size; this is an issue with your terminal emulator.\nPlease use a different terminal emulator or report this bug to tdf.".into()
			)),
			e => Err(WrappedErr(format!("Couldn't get the necessary information to set up images: {e}").into()))
		})?;

	// then we want to spawn off the rendering task
	// We need to use the thread::spawn API so that this exists in a thread not owned by tokio,
	// since the methods we call in `start_rendering` will panic if called in an async context
	let prerender = flags
		.prerender
		.and_then(NonZeroUsize::new)
		.map_or(PrerenderLimit::All, PrerenderLimit::Limited);

	let file_path = path.clone();
	std::thread::spawn(move || {
		renderer::start_rendering(
			&file_path,
			render_tx,
			render_rx,
			cell_height_px,
			cell_width_px,
			prerender,
			black,
			white
		)
	});

	let font_size = picker.font_size();

	let mut ev_stream = crossterm::event::EventStream::new();

	let (to_converter, from_main) = flume::unbounded();
	let (to_main, from_converter) = flume::unbounded();

	let is_kitty = picker.protocol_type() == ProtocolType::Kitty;
	let is_tmux = picker.is_tmux() || std::env::var_os("TMUX").is_some();
	let tmux_anchor = if is_tmux {
		Some(query_tmux_anchor()?)
	} else {
		None
	};
	let tmux_pane_active = if is_tmux {
		query_tmux_pane_active().unwrap_or(true)
	} else {
		true
	};

	let shms_work = is_kitty && !is_tmux && do_shms_work(false, &mut ev_stream).await;
	if is_kitty && is_tmux {
		drain_pending_input_events();
	}

	tokio::spawn(run_conversion_loop(
		to_main,
		from_main,
		picker,
		20,
		shms_work,
		is_kitty
	));

	let file_name = path.file_name().map_or_else(
		|| "Unknown file".into(),
		|n| n.to_string_lossy().to_string()
	);
	let tui = Tui::new(file_name, flags.max_wide, flags.r_to_l, is_kitty);

	let backend = CrosstermBackend::new(std::io::stdout());
	let mut term = Terminal::new(backend).map_err(|e| {
		WrappedErr(format!("Couldn't set up crossterm's terminal backend: {e}").into())
	})?;
	term.skip_diff(true);

	enable_raw_mode().map_err(|e| {
		WrappedErr(
			format!("Can't enable raw mode, which is necessary to receive input: {e}").into()
		)
	})?;

	if is_kitty {
		run_action(
			Action::Delete(DeleteConfig {
				effect: ClearOrDelete::Delete,
				which: WhichToDelete::IdRange(NonZeroU32::new(1).unwrap()..=NonZeroU32::MAX)
			}),
			is_tmux,
			&mut ev_stream
		)
		.await
		.map_err(|e| {
			WrappedErr(format!("Couldn't delete all previous images from memory: {e}").into())
		})?;
	}

	let fullscreen = flags.fullscreen;
	let main_area = Tui::main_layout(&term.get_frame(), fullscreen);
	to_renderer
		.send(RenderNotif::Area(main_area.page_area))
		.map_err(|e| {
			WrappedErr(
				format!("Couldn't inform the rendering thread of the available area: {e}").into()
			)
		})?;

	let tui_rx = tui_rx.into_stream();
	let from_converter = from_converter.into_stream();

	enter_redraw_loop(
		ev_stream,
		to_renderer,
		tui_rx,
		to_converter,
		from_converter,
		fullscreen,
		is_tmux,
		is_kitty,
		tmux_anchor,
		tmux_pane_active,
		tui,
		&mut term,
		main_area,
		font_size
	)
	.await
	.map_err(|e| {
		WrappedErr(
			format!(
				"An unexpected error occurred while communicating between different parts of tdf: {e}"
			)
			.into()
		)
	})?;

	drop(maybe_logger);
	Ok(())
}

// oh shut up clippy who cares
#[expect(clippy::too_many_arguments)]
async fn enter_redraw_loop(
	mut ev_stream: EventStream,
	to_renderer: Sender<RenderNotif>,
	mut tui_rx: RecvStream<'_, Result<RenderInfo, RenderError>>,
	to_converter: Sender<ConverterMsg>,
	mut from_converter: RecvStream<'_, Result<ConvertedPage, RenderError>>,
	mut fullscreen: bool,
	is_tmux: bool,
	is_kitty: bool,
	mut tmux_anchor: Option<TmuxAnchor>,
	mut tmux_pane_active: bool,
	mut tui: Tui,
	term: &mut Terminal<CrosstermBackend<Stdout>>,
	mut main_area: tdf::tui::RenderLayout,
	font_size: FontSize
) -> Result<(), Box<dyn Error>> {
	let mut kitty_placement_state = KittyPlacementState::default();
	loop {
		let mut needs_redraw = false;
		let mut should_quit = false;
		let next_ev = ev_stream.next().fuse();
		tokio::select! {
		// First we check if we have any keystrokes
			Some(ev) = next_ev => {
				// If we can't get user input, just crash.
				let ev = ev.expect("Couldn't get any user input");
				if is_tmux && is_kitty {
						match ev {
								CrosstermEvent::FocusLost => {
								let _ = run_action(
								Action::Delete(DeleteConfig {
									effect: ClearOrDelete::Clear,
									which: WhichToDelete::All
								}),
								true,
								&mut ev_stream
									)
									.await;
									kitty_placement_state.invalidate_tmux();
									drain_pending_input_events();
									continue;
							}
						CrosstermEvent::FocusGained => {
							tui.invalidate_render_cache();
							continue;
						}
						_ => (),
					}
				}

				match tui.handle_event(&ev) {
					None => (),
					Some(action) => match action {
						InputAction::Redraw => needs_redraw = true,
						InputAction::QuitApp => should_quit = true,
						InputAction::JumpingToPage(page) => {
							to_renderer.send(RenderNotif::JumpToPage(page))?;
							to_converter.send(ConverterMsg::GoToPage(page))?;
							needs_redraw = true;
						},
						InputAction::Search(term) => {
							to_renderer.send(RenderNotif::Search(term))?;
							needs_redraw = true;
						}
						InputAction::Invert => {
							to_renderer.send(RenderNotif::Invert)?;
							needs_redraw = true;
						}
						InputAction::Rotate => {
							to_renderer.send(RenderNotif::Rotate)?;
							needs_redraw = true;
						}
						InputAction::Fullscreen => {
							fullscreen = !fullscreen;
							needs_redraw = true;
						}
						InputAction::SwitchRenderZoom(f_or_f) => {
							to_renderer.send(RenderNotif::SwitchFitOrFill(f_or_f)).unwrap();
							needs_redraw = true;
						}
					}
				}
		},
		Some(renderer_msg) = tui_rx.next() => {
			match renderer_msg {
					Ok(render_info) => match render_info {
						RenderInfo::NumPages(num) => {
							tui.set_n_pages(num);
							to_converter.send(ConverterMsg::NumPages(num))?;
							needs_redraw = true;
						},
							RenderInfo::Page(info) => {
								tui.got_num_results_on_page(info.page_num, info.result_rects.len());
								let should_redraw = tui.is_page_visible(info.page_num);
								to_converter.send(ConverterMsg::AddImg(info))?;
								needs_redraw |= should_redraw;
							},
						RenderInfo::Reloaded => {
							tui.set_msg(MessageSetting::Some(BottomMessage::Reloaded));
							needs_redraw = true;
						}
						RenderInfo::SearchResults { page_num, num_results } =>
							{
								tui.got_num_results_on_page(page_num, num_results);
								needs_redraw = true;
							},
					},
					Err(e) => {
						tui.show_error(e);
						needs_redraw = true;
					}
				}
			}
			Some(img_res) = from_converter.next() => {
				match img_res {
					Ok(ConvertedPage { page, num, num_results }) => {
						tui.page_ready(page, num, num_results);
						if num == tui.page {
							needs_redraw = true;
						}
					},
					Err(e) => {
						tui.show_error(e);
						needs_redraw = true;
					}
				}
			},
		};

		if should_quit {
			if is_kitty {
				let _ = run_action(
					Action::Delete(DeleteConfig {
						effect: ClearOrDelete::Delete,
						which: WhichToDelete::IdRange(
							NonZeroU32::new(1).unwrap()..=NonZeroU32::MAX
						)
					}),
					is_tmux,
					&mut ev_stream
				)
				.await;
				let _ = run_action(
					Action::Delete(DeleteConfig {
						effect: ClearOrDelete::Delete,
						which: WhichToDelete::All
					}),
					is_tmux,
					&mut ev_stream
				)
				.await;
				if is_tmux {
					kitty_placement_state.invalidate_tmux();
					drain_pending_input_events();
				}
			}
			return Ok(());
		}

		let new_area = Tui::main_layout(&term.get_frame(), fullscreen);
		if new_area != main_area {
			main_area = new_area;
			to_renderer.send(RenderNotif::Area(main_area.page_area))?;
			needs_redraw = true;
			if is_tmux {
				let new_anchor = query_tmux_anchor().map_err(|e| Box::new(e) as Box<dyn Error>)?;
				if tmux_anchor != Some(new_anchor) {
					kitty_placement_state.invalidate_tmux();
				}
				tmux_anchor = Some(new_anchor);
			}
		}

		if is_tmux && is_kitty {
			match query_tmux_pane_active() {
				Ok(false) => {
					if tmux_pane_active {
						let _ = run_action(
							Action::Delete(DeleteConfig {
								effect: ClearOrDelete::Clear,
								which: WhichToDelete::All
							}),
							true,
							&mut ev_stream
						)
						.await;
						kitty_placement_state.invalidate_tmux();
						drain_pending_input_events();
						tmux_pane_active = false;
					}
					continue;
				}
				Ok(true) => {
					if !tmux_pane_active {
						kitty_placement_state.invalidate_tmux();
						tui.invalidate_render_cache();
						needs_redraw = true;
					}
					tmux_pane_active = true;
				}
				Err(_) => ()
			}
		}

		if needs_redraw {
			if is_tmux {
				match query_tmux_anchor() {
					Ok(anchor) => {
						if tmux_anchor != Some(anchor) {
							kitty_placement_state.invalidate_tmux();
						}
						tmux_anchor = Some(anchor);
					}
					Err(e) if tmux_anchor.is_none() => return Err(Box::new(e) as Box<dyn Error>),
					Err(_) => ()
				}
			}

			let mut to_display = KittyDisplay::NoChange;
			term.draw(|f| {
				to_display = tui.render(f, &main_area, font_size);
			})?;

			let maybe_err = display_kitty_images(
				to_display,
				is_tmux,
				tmux_anchor,
				&mut kitty_placement_state,
				&mut ev_stream
			)
			.await;

			if let Err(DisplayErr {
				page_nums_failed_to_transfer,
				user_facing_err,
				source
			}) = maybe_err
			{
				match source {
					// This is the error that kitty & ghostty provide us when they delete an
					// image due to memory constraints, so if we get it, we just fix it by
					// re-rendering so it don't display it to the user
					//
					// [TODO] maybe when we detect that an image was deleted, we probe the
					// terminal for the pages around it to see if they were deleted too and if
					// they were, we re-render them? idk
					TransmitError::Terminal(TerminalError::NoEntity(_)) => (),
					_ => {
						if is_tmux {
							let _ = run_action(
								Action::Delete(DeleteConfig {
									effect: ClearOrDelete::Delete,
									which: WhichToDelete::All
								}),
								is_tmux,
								&mut ev_stream
							)
							.await;
							kitty_placement_state.invalidate_tmux();
							return Err(format!("{user_facing_err}: {source}").into());
						}
						tui.set_msg(MessageSetting::Some(BottomMessage::Error(format!(
							"{user_facing_err}: {source}"
						))));
					}
				}

				for page_num in page_nums_failed_to_transfer {
					tui.page_failed_display(page_num);
					// So that they get re-rendered and sent over again
					to_renderer.send(RenderNotif::PageNeedsReRender(page_num))?;
				}
			}

			execute!(stdout().lock(), EndSynchronizedUpdate)?;
		}
	}
}

fn on_notify_ev(
	to_tui_tx: flume::Sender<Result<RenderInfo, RenderError>>,
	to_render_tx: flume::Sender<RenderNotif>,
	file_name: OsString,
	debounce_delay: Duration
) -> impl Fn(notify::Result<Event>) {
	let last_event: Mutex<Result<(), RenderError>> = Mutex::new(Ok(()));
	let last_event = Arc::new(last_event);

	let debouncer = EventDebouncer::new(debounce_delay, {
		let last_event = last_event.clone();
		move |()| {
			let event = mem::replace(&mut *last_event.lock().unwrap(), Ok(()));
			match event {
				// This shouldn't fail to send unless the receiver gets disconnected. If that's
				// happened, then like the main thread has panicked or something, so it doesn't matter
				// we don't handle the error here.
				Ok(()) => to_render_tx.send(RenderNotif::Reload).unwrap(),
				// If we get an error here, and then an error sending, everything's going wrong. Just give
				// up lol.
				Err(e) => to_tui_tx.send(Err(e)).unwrap()
			}
		}
	});

	move |res| {
		let event = match res {
			Err(e) => Err(RenderError::Notify(e)),

			// TODO: Should we match EventKind::Rename and propogate that so that the other parts of the
			// process know that too? Or should that be
			Ok(ev) => {
				// We only watch the parent directory (see the comment above `watcher.watch` in `fn
				// main`) so we need to filter out events to only ones that pertain to the single file
				// we care about
				if !ev
					.paths
					.iter()
					.any(|path| path.file_name().is_some_and(|f| f == file_name))
				{
					return;
				}

				match ev.kind {
					EventKind::Access(_) => return,
					EventKind::Remove(_) => Err(RenderError::Converting("File was deleted".into())),
					EventKind::Other
					| EventKind::Any
					| EventKind::Create(_)
					| EventKind::Modify(_) => Ok(())
				}
			}
		};
		*last_event.lock().unwrap() = event;
		debouncer.put(());
	}
}

fn parse_color_to_i32(cs: &str) -> Result<i32, csscolorparser::ParseColorError> {
	let color = csscolorparser::parse(cs)?;
	let [r, g, b, _] = color.to_rgba8();
	Ok(i32::from_be_bytes([0, r, g, b]))
}

fn get_font_size_through_stdio() -> Result<(u16, u16), WrappedErr> {
	// send the command code to get the terminal window size
	print!("\x1b[14t");
	std::io::stdout().flush().unwrap();

	// we need to enable raw mode here since this bit of output won't print a newline; it'll
	// just print the info it wants to tell us. So we want to get all characters as they come
	enable_raw_mode().map_err(|e| {
		WrappedErr(
			format!("Can't enable raw mode, which is necessary to receive input: {e}").into()
		)
	})?;

	// read in the returned size until we hit a 't' (which indicates to us it's done)
	let input_vec = BufReader::new(std::io::stdin())
		.bytes()
		.filter_map(Result::ok)
		.take_while(|b| *b != b't')
		.collect::<Vec<_>>();

	// and then disable raw mode again in case we return an error in this next section
	disable_raw_mode().map_err(|e| {
		WrappedErr(format!("Can't put the terminal back into a normal input state: {e}").into())
	})?;

	let input_line = String::from_utf8(input_vec).map_err(|e| {
		WrappedErr(
			format!(
				"The terminal responded to our request for its font size by providing non-utf8 data: {e}"
			)
			.into()
		)
	})?;
	let input_line = input_line
		.trim_start_matches("\x1b[4")
		.trim_start_matches(';');

	// it should input it to us as `\e[4;<height>;<width>t`, so we need to split to get the h/w
	// ignore the first val
	let mut splits = input_line.split([';', 't']);

	let (Some(h), Some(w)) = (splits.next(), splits.next()) else {
		return Err(WrappedErr(
			format!("Terminal responded with unparseable size response '{input_line}'").into()
		));
	};

	let h = h.parse::<u16>().map_err(|e| {
		WrappedErr(
			format!(
				"Your terminal said its height is {h}, but that is not a 16-bit unsigned integer: {e}"
			)
			.into()
		)
	})?;
	let w = w.parse::<u16>().map_err(|e| {
		WrappedErr(
			format!(
				"Your terminal said its width is {w}, but that is not a 16-bit unsigned integer: {e}"
			)
			.into()
		)
	})?;

	Ok((w, h))
}
