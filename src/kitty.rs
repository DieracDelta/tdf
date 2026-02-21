use std::{collections::HashMap, io::Write, num::NonZeroU32};

use crossterm::{
	cursor::MoveTo,
	event::EventStream,
	execute,
	terminal::{disable_raw_mode, enable_raw_mode}
};
use image::DynamicImage;
use kittage::{
	AsyncInputReader, ImageDimensions, ImageId, NumberOrId, PixelFormat, Verbosity,
	action::{Action, NONZERO_ONE},
	delete::{ClearOrDelete, DeleteConfig, WhichToDelete},
	display::{CursorMovementPolicy, DisplayConfig, DisplayLocation},
	error::TransmitError,
	image::Image,
	medium::Medium
};
use ratatui::layout::Position;

use crate::converter::MaybeTransferred;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TmuxAnchor {
	pub window_offset_x: u16,
	pub window_offset_y: u16,
	pub pane_left: u16,
	pub pane_top: u16,
	pub pane_width: u16,
	pub pane_height: u16
}

pub struct KittyReadyToDisplay<'tui> {
	pub img: &'tui mut MaybeTransferred,
	pub page_num: usize,
	pub pos: Position,
	pub display_loc: DisplayLocation
}

#[derive(Default)]
pub struct KittyPlacementState {
	active_slots: HashMap<usize, (usize, ImageId)>
}

impl KittyPlacementState {
	pub fn invalidate_tmux(&mut self) {
		self.active_slots.clear();
	}
}

pub enum KittyDisplay<'tui> {
	NoChange,
	ClearImages,
	DisplayImages(Vec<KittyReadyToDisplay<'tui>>)
}

pub struct DisplayErr<E> {
	pub page_nums_failed_to_transfer: Vec<usize>,
	pub user_facing_err: &'static str,
	pub source: E
}

impl<E> DisplayErr<E> {
	pub fn new(
		page_nums_failed_to_transfer: Vec<usize>,
		user_facing_err: &'static str,
		source: E
	) -> Self {
		Self {
			page_nums_failed_to_transfer,
			user_facing_err,
			source
		}
	}

	fn new_no_imgs(user_facing_err: &'static str, source: E) -> Self {
		Self::new(vec![], user_facing_err, source)
	}
}

pub struct DbgWriter<W: Write> {
	w: W,
	#[cfg(debug_assertions)]
	buf: String
}

impl<W: Write> Write for DbgWriter<W> {
	fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
		#[cfg(debug_assertions)]
		{
			if let Ok(s) = std::str::from_utf8(buf) {
				self.buf.push_str(s);
			}
		}
		self.w.write(buf)
	}

	fn flush(&mut self) -> std::io::Result<()> {
		#[cfg(debug_assertions)]
		{
			log::debug!("Writing to kitty: {:?}", self.buf);
			self.buf.clear();
		}
		self.w.flush()
	}
}

pub struct TdfTmuxWriter<W: Write> {
	inner: W,
	in_passthrough: bool,
	last_was_esc: bool
}

impl<W: Write> TdfTmuxWriter<W> {
	fn new(inner: W) -> Self {
		Self {
			inner,
			in_passthrough: false,
			last_was_esc: false
		}
	}

	fn open_passthrough(&mut self) -> std::io::Result<()> {
		if !self.in_passthrough {
			self.inner.write_all(b"\x1bPtmux;")?;
			self.in_passthrough = true;
		}
		Ok(())
	}

	fn close_passthrough(&mut self) -> std::io::Result<()> {
		if self.in_passthrough {
			self.inner.write_all(b"\x1b\\")?;
			self.in_passthrough = false;
			self.last_was_esc = false;
		}
		Ok(())
	}
}

impl<W: Write> Write for TdfTmuxWriter<W> {
	fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
		if buf.is_empty() {
			return Ok(0);
		}

		for &b in buf {
			self.open_passthrough()?;

			if b == 0x1b {
				self.inner.write_all(&[0x1b, 0x1b])?;
				self.last_was_esc = true;
				continue;
			}

			self.inner.write_all(&[b])?;

			if self.last_was_esc && b == b'\\' {
				self.close_passthrough()?;
			}

			self.last_was_esc = false;
		}

		Ok(buf.len())
	}

	fn flush(&mut self) -> std::io::Result<()> {
		self.close_passthrough()?;
		self.inner.flush()
	}
}

pub async fn run_action<'es>(
	action: Action<'_, '_>,
	is_tmux: bool,
	ev_stream: &'es mut EventStream
) -> Result<ImageId, TransmitError<<&'es mut EventStream as AsyncInputReader>::Error>> {
	let fallback_img_id = match &action {
		Action::Display { image_id, .. } => Some(*image_id),
		Action::Transmit(img) | Action::TransmitAndDisplay { image: img, .. } =>
			match img.num_or_id {
				NumberOrId::Id(id) => Some(id),
				NumberOrId::Number(_) => None
			},
		Action::Query(img) => match img.num_or_id {
			NumberOrId::Id(id) => Some(id),
			NumberOrId::Number(_) => None
		},
		Action::Delete(_) => Some(NONZERO_ONE)
	};

	let writer = DbgWriter {
		w: std::io::stdout().lock(),
		#[cfg(debug_assertions)]
		buf: String::new()
	};

	if is_tmux && !matches!(action, Action::Query(_)) {
		let writer = action
			.write_transmit_to(TdfTmuxWriter::new(writer), Verbosity::Silent)
			.map_err(TransmitError::Writing)?;
		drop(writer);
		return fallback_img_id.ok_or_else(|| {
			TransmitError::Writing(std::io::Error::other(
				"tmux non-query action missing deterministic image id"
			))
		});
	}

	if is_tmux {
		action
			.execute_async(TdfTmuxWriter::new(writer), ev_stream)
			.await
			.map(|(_, i)| i)
	} else {
		action
			.execute_async(writer, ev_stream)
			.await
			.map(|(_, i)| i)
	}
}

pub async fn do_shms_work(is_tmux: bool, ev_stream: &mut EventStream) -> bool {
	let img = DynamicImage::new_rgb8(1, 1);
	let pid = std::process::id();
	let Ok(mut k_img) = kittage::image::Image::shm_from(img, &format!("tdf_test_{pid}")) else {
		return false;
	};

	// apparently the terminal won't respond to queries unless they have an Id instead of a number
	k_img.num_or_id = NumberOrId::Id(NonZeroU32::new(u32::MAX).unwrap());

	enable_raw_mode().unwrap();

	let res = run_action(Action::Query(&k_img), is_tmux, ev_stream).await;

	disable_raw_mode().unwrap();

	res.is_ok()
}

pub async fn display_kitty_images<'es>(
	display: KittyDisplay<'_>,
	is_tmux: bool,
	tmux_anchor: Option<TmuxAnchor>,
	placement_state: &mut KittyPlacementState,
	ev_stream: &'es mut EventStream
) -> Result<(), DisplayErr<TransmitError<<&'es mut EventStream as AsyncInputReader>::Error>>> {
	const TMUX_SLOT_PLACEMENT_BASE_RAW: u32 = u32::MAX - 65536;

	fn slot_placement_id(slot_idx: usize) -> Option<ImageId> {
		let slot = u32::try_from(slot_idx).ok()?;
		TMUX_SLOT_PLACEMENT_BASE_RAW
			.checked_add(slot)
			.and_then(NonZeroU32::new)
	}

	fn move_cursor_tmux(global_x: u16, global_y: u16) -> std::io::Result<()> {
		let mut writer = TdfTmuxWriter::new(std::io::stdout().lock());
		// CSI uses 1-based coordinates.
		write!(
			writer,
			"\x1b[{};{}H",
			usize::from(global_y).saturating_add(1),
			usize::from(global_x).saturating_add(1)
		)?;
		writer.flush()
	}

	let images = match display {
		KittyDisplay::NoChange => return Ok(()),
		KittyDisplay::ClearImages => {
			run_action(
				Action::Delete(DeleteConfig {
					effect: ClearOrDelete::Clear,
					which: WhichToDelete::All
				}),
				is_tmux,
				ev_stream
			)
			.await
			.map_err(|e| DisplayErr::new_no_imgs("Couldn't clear previous images", e))?;
			if is_tmux {
				placement_state.invalidate_tmux();
			}
			return Ok(());
		}
			KittyDisplay::DisplayImages(images) => images
		};

	let mut err = None;
	let mut desired_slots: HashMap<usize, (usize, ImageId)> = HashMap::new();
	for KittyReadyToDisplay {
		img,
		page_num,
		pos,
		mut display_loc
	} in images
	{
		let mut placement_id_for_display = None;
		if is_tmux {
			let Some(anchor) = tmux_anchor else {
				return Err(DisplayErr::new_no_imgs(
					"Couldn't determine tmux pane offsets for kitty placement",
					TransmitError::Writing(std::io::Error::other(
						"missing tmux anchor for tmux display"
					))
				));
			};

			if pos.x >= anchor.pane_width || pos.y >= anchor.pane_height {
				continue;
			}

			let max_cols = anchor.pane_width.saturating_sub(pos.x);
			let max_rows = anchor.pane_height.saturating_sub(pos.y);
			if display_loc.columns == 0 || display_loc.columns > max_cols {
				display_loc.columns = max_cols;
			}
			if display_loc.rows == 0 || display_loc.rows > max_rows {
				display_loc.rows = max_rows;
			}
		}

		if !is_tmux {
			execute!(std::io::stdout(), MoveTo(pos.x, pos.y)).unwrap();
		}

		let config = DisplayConfig {
			location: display_loc,
			cursor_movement: CursorMovementPolicy::DontMove,
			..DisplayConfig::default()
		};

		let slot_idx = desired_slots.len();
		if is_tmux {
			let Some(anchor) = tmux_anchor else {
				return Err(DisplayErr::new_no_imgs(
					"Couldn't determine tmux pane offsets for kitty placement",
					TransmitError::Writing(std::io::Error::other(
						"missing tmux anchor for tmux display"
					))
				));
			};
			let global_x = anchor
				.window_offset_x
				.saturating_add(anchor.pane_left)
				.saturating_add(pos.x);
			let global_y = anchor
				.window_offset_y
				.saturating_add(anchor.pane_top)
				.saturating_add(pos.y);
			move_cursor_tmux(global_x, global_y)
				.map_err(TransmitError::Writing)
				.map_err(|e| {
					DisplayErr::new_no_imgs("Couldn't move cursor in tmux passthrough", e)
				})?;
			placement_id_for_display = slot_placement_id(slot_idx);
		}

		log::debug!("going to display img {img:#?}");
		log::debug!("displaying with config {config:#?}");

		let this_err = match img {
			MaybeTransferred::NotYet(image) => {
				let mut fake_image = Image {
					num_or_id: image.num_or_id,
					format: PixelFormat::Rgb24(
						ImageDimensions {
							width: 0,
							height: 0
						},
						None
					),
					medium: Medium::Direct {
						chunk_size: None,
						data: (&[]).into()
					}
				};
				std::mem::swap(image, &mut fake_image);

				let res = match run_action(Action::Transmit(fake_image), is_tmux, ev_stream).await {
					Ok(img_id) =>
						run_action(
							Action::Display {
								image_id: img_id,
								placement_id: placement_id_for_display.unwrap_or(img_id),
								config
							},
							is_tmux,
							ev_stream
						)
						.await,
					Err(e) => Err(e)
				};

				match res {
					Ok(img_id) => {
						*img = MaybeTransferred::Transferred(img_id);
						Ok(())
					}
					Err(e) => Err((page_num, e))
				}
			}
			MaybeTransferred::Transferred(image_id) => run_action(
				Action::Display {
					image_id: *image_id,
					placement_id: placement_id_for_display.unwrap_or(*image_id),
					config
				},
				is_tmux,
				ev_stream
			)
			.await
			.map(|_| ())
			.map_err(|e| (page_num, e))
		};

		log::debug!("this_err is {this_err:#?}");

		match this_err {
			Ok(()) => {
				let image_id = match img {
					MaybeTransferred::Transferred(id) => *id,
					MaybeTransferred::NotYet(_) => continue
				};
				desired_slots.insert(slot_idx, (page_num, image_id));
			}
			Err((id, e)) => {
				let e = err.get_or_insert_with(|| (vec![], e));
				e.0.push(id);
			}
		}
	}

	if is_tmux {
		for (slot_idx, (old_page, old_img)) in placement_state.active_slots.clone() {
			match desired_slots.get(&slot_idx) {
				Some((new_page, new_img)) if *new_page == old_page && *new_img == old_img => (),
				_ => {
					let slot_pid = slot_placement_id(slot_idx).unwrap_or(old_img);
					let _ = run_action(
						Action::Delete(DeleteConfig {
							effect: ClearOrDelete::Clear,
							which: WhichToDelete::ImageId(old_img, Some(slot_pid))
						}),
						true,
						ev_stream
					)
					.await;
				}
			}
		}
		placement_state.active_slots = desired_slots;
	}

	match err {
		Some((replace, e)) => Err(DisplayErr::new(
			replace,
			"Couldn't transfer image to the terminal",
			e
		)),
		None => Ok(())
	}
}
