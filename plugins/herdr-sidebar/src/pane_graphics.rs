//! Pixel-exact image placement through herdr's experimental pane graphics API
//! (`[experimental] kitty_graphics = true`).
//!
//! The viewer's portable half-block renderer gives two pixels per terminal
//! cell, which turns screenshots and document pages into mosaics. When herdr
//! advertises pane graphics, the viewer instead sends the image at the pane's
//! real pixel resolution over a `pane.graphics.stream` socket and lets the
//! terminal scale it into a cell rectangle. herdr owns the layer: it replays
//! the placement across redraws and tab switches, and removes it as soon as
//! the stream socket closes — including when the viewer process dies — so no
//! image can outlive the preview that drew it.
//!
//! Streams are unix-only: a synchronous Windows named-pipe handle serializes a
//! blocked read with every write, so the failure reader below would stall the
//! frames. Windows keeps the half-block renderer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// herdr rejects inline stream frames above 16 MiB
/// (`PANE_GRAPHICS_STREAM_MAX_BYTES`).
pub const FRAME_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Pixel size of one terminal cell, as reported by the attached client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellSize {
    pub width_px: u32,
    pub height_px: u32,
}

/// A rectangle of pane cells (0-based, relative to the pane's viewport).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellRect {
    pub col: u16,
    pub row: u16,
    pub cols: u16,
    pub rows: u16,
}

/// How one image is laid out: the cells it covers, and the frame sent for
/// them. The terminal stretches the frame over the cells, so the frame keeps
/// exactly the cells' aspect ratio and the image sits centered inside it with
/// transparent padding (at most one cell per axis) — never distorted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FramePlan {
    pub placement: CellRect,
    pub canvas: (u32, u32),
    pub image_at: (u32, u32),
    pub image_size: (u32, u32),
}

/// Largest cell size treated as real. A client reporting nonsense must not
/// make the frame arithmetic overflow.
const MAX_CELL_PX: u32 = 1000;

/// Cell size out of a `pane.graphics.info` response; `None` for an error
/// response (`feature_disabled` when kitty graphics are off) or a client that
/// cannot report usable pixels.
pub fn parse_info(response: &str) -> Option<CellSize> {
    let value: serde_json::Value = serde_json::from_str(response.trim()).ok()?;
    let result = value.get("result")?;
    let dimension = |key: &str| {
        result
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .and_then(|px| u32::try_from(px).ok())
            .filter(|px| (1..=MAX_CELL_PX).contains(px))
    };
    Some(CellSize {
        width_px: dimension("cell_width_px")?,
        height_px: dimension("cell_height_px")?,
    })
}

/// Ask herdr for the pane's cell size. `None` means "no pane graphics": the
/// caller renders half blocks exactly as before.
pub fn probe(pane_id: &str) -> Option<CellSize> {
    if !cfg!(unix) || pane_id.is_empty() {
        return None;
    }
    let response = crate::ipc::call_text(
        "pane.graphics.info",
        serde_json::json!({ "pane_id": pane_id }),
    )
    .ok()?;
    parse_info(&response)
}

/// Fit an `image`-pixel image into `area`, centered, preserving its aspect
/// ratio. The frame is rendered at the display resolution, except that a
/// small image is never upsampled here (the terminal scales it) and a frame
/// never exceeds `max_bytes` of RGBA.
pub fn plan(
    area: CellRect,
    cell: CellSize,
    image: (u32, u32),
    max_bytes: u64,
) -> Option<FramePlan> {
    let (image_w, image_h) = image;
    if area.cols == 0 || area.rows == 0 || image_w == 0 || image_h == 0 {
        return None;
    }
    let (cell_w, cell_h) = (f64::from(cell.width_px), f64::from(cell.height_px));
    if cell_w == 0.0 || cell_h == 0.0 {
        return None;
    }
    let box_w = f64::from(area.cols) * cell_w;
    let box_h = f64::from(area.rows) * cell_h;
    let scale = (box_w / f64::from(image_w)).min(box_h / f64::from(image_h));
    let fitted_w = f64::from(image_w) * scale;
    let fitted_h = f64::from(image_h) * scale;
    // The epsilon keeps an exact fit (1394.0000001 px) from spilling into
    // one more cell.
    let cells =
        |fitted: f64, cell: f64, limit: u16| ((fitted / cell - 1e-6).ceil() as u16).clamp(1, limit);
    let cols = cells(fitted_w, cell_w, area.cols);
    let rows = cells(fitted_h, cell_h, area.rows);
    let placement = CellRect {
        col: area.col + (area.cols - cols) / 2,
        row: area.row + (area.rows - rows) / 2,
        cols,
        rows,
    };
    let display_w = f64::from(cols) * cell_w;
    let display_h = f64::from(rows) * cell_h;
    let mut resolution = (1.0 / scale).min(1.0);
    loop {
        let canvas_w = ((display_w * resolution).round() as u32).max(1);
        let canvas_h = ((display_h * resolution).round() as u32).max(1);
        if u64::from(canvas_w)
            .saturating_mul(u64::from(canvas_h))
            .saturating_mul(4)
            <= max_bytes
            || (canvas_w, canvas_h) == (1, 1)
        {
            let image_size = (
                ((fitted_w * resolution).round() as u32).clamp(1, canvas_w),
                ((fitted_h * resolution).round() as u32).clamp(1, canvas_h),
            );
            return Some(FramePlan {
                placement,
                canvas: (canvas_w, canvas_h),
                image_at: ((canvas_w - image_size.0) / 2, (canvas_h - image_size.1) / 2),
                image_size,
            });
        }
        resolution *= 0.95;
    }
}

/// Render `source` into the planned frame.
pub fn compose(plan: &FramePlan, source: &image::RgbaImage) -> image::RgbaImage {
    let scaled;
    let image = if source.dimensions() == plan.image_size {
        source
    } else {
        scaled = image::imageops::resize(
            source,
            plan.image_size.0,
            plan.image_size.1,
            image::imageops::FilterType::CatmullRom,
        );
        &scaled
    };
    if plan.canvas == plan.image_size {
        return image.clone();
    }
    let mut canvas = image::RgbaImage::new(plan.canvas.0, plan.canvas.1);
    image::imageops::replace(
        &mut canvas,
        image,
        i64::from(plan.image_at.0),
        i64::from(plan.image_at.1),
    );
    canvas
}

/// Header line for one inline RGBA frame of a `pane.graphics.stream`.
#[cfg_attr(not(unix), allow(dead_code))]
fn frame_header(plan: &FramePlan, data_length: usize) -> String {
    serde_json::json!({
        "format": "rgba",
        "image_width": plan.canvas.0,
        "image_height": plan.canvas.1,
        "data_length": data_length,
        "placement": {
            "viewport_col": plan.placement.col,
            "viewport_row": plan.placement.row,
            "grid_cols": plan.placement.cols,
            "grid_rows": plan.placement.rows,
        },
    })
    .to_string()
}

/// How often opening a stream is retried, and how long between attempts.
#[cfg(unix)]
const OPEN_ATTEMPTS: u32 = 3;
#[cfg(unix)]
const OPEN_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(40);

/// One open graphics layer on a pane. Dropping it closes the socket, and
/// herdr removes the layer.
pub struct Stream {
    #[cfg(unix)]
    socket: std::os::unix::net::UnixStream,
    /// Set by the reader thread when herdr answers a frame (it only answers
    /// to reject one) or closes the stream.
    failed: Arc<AtomicBool>,
}

impl Stream {
    /// herdr refuses a second stream while the previous owner's layer is
    /// still being torn down (`stream_conflict`), which is exactly what a
    /// quick image → text → image switch does. Retry briefly; this runs on a
    /// worker thread, never on the draw path.
    #[cfg(unix)]
    pub fn open(pane_id: &str) -> Option<Self> {
        for attempt in 0..OPEN_ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(OPEN_RETRY_DELAY);
            }
            if let Some(stream) = Self::open_once(pane_id) {
                return Some(stream);
            }
        }
        None
    }

    #[cfg(unix)]
    fn open_once(pane_id: &str) -> Option<Self> {
        use std::io::{Read as _, Write as _};
        let path = crate::ipc::socket_path()?;
        let mut socket = std::os::unix::net::UnixStream::connect(path).ok()?;
        let timeout = Some(std::time::Duration::from_secs(5));
        socket.set_read_timeout(timeout).ok()?;
        socket.set_write_timeout(timeout).ok()?;
        let request = serde_json::json!({
            "id": "herdr-sidebar:pane.graphics.stream",
            "method": "pane.graphics.stream",
            "params": { "pane_id": pane_id, "z_index": 0 },
        });
        socket.write_all(format!("{request}\n").as_bytes()).ok()?;
        // Byte-wise so nothing after the acknowledgement is buffered away
        // from the failure reader.
        let mut ack = Vec::new();
        let mut byte = [0u8; 1];
        while ack.len() < 64 * 1024 {
            match socket.read(&mut byte) {
                Ok(1) if byte[0] == b'\n' => break,
                Ok(1) => ack.push(byte[0]),
                _ => return None,
            }
        }
        let ack: serde_json::Value = serde_json::from_slice(&ack).ok()?;
        if ack
            .pointer("/result/type")
            .and_then(serde_json::Value::as_str)
            != Some("ok")
        {
            return None;
        }
        socket.set_read_timeout(None).ok()?;
        let failed = Arc::new(AtomicBool::new(false));
        let mut reader = socket.try_clone().ok()?;
        let flag = Arc::clone(&failed);
        std::thread::spawn(move || {
            let mut buffer = [0u8; 512];
            // Any byte (an error line) or EOF means the layer is gone.
            let _ = reader.read(&mut buffer);
            flag.store(true, Ordering::Release);
        });
        Some(Self { socket, failed })
    }

    #[cfg(not(unix))]
    pub fn open(_pane_id: &str) -> Option<Self> {
        None
    }

    /// Replace the layer's image. `false` when the stream is unusable.
    pub fn send(&mut self, plan: &FramePlan, frame: &image::RgbaImage) -> bool {
        if self.failed() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::io::Write as _;
            let data = frame.as_raw();
            let header = frame_header(plan, data.len());
            self.socket
                .write_all(format!("{header}\n").as_bytes())
                .and_then(|()| self.socket.write_all(data))
                .and_then(|()| self.socket.flush())
                .is_ok()
        }
        #[cfg(not(unix))]
        {
            let _ = (plan, frame);
            false
        }
    }

    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // The reader thread holds a clone of the socket: shut the connection
        // down (not just this handle) so herdr sees EOF and clears the layer.
        #[cfg(unix)]
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RETINA: CellSize = CellSize {
        width_px: 17,
        height_px: 35,
    };

    fn area(cols: u16, rows: u16) -> CellRect {
        CellRect {
            col: 0,
            row: 1,
            cols,
            rows,
        }
    }

    #[test]
    fn info_yields_the_cell_size_and_errors_disable_graphics() {
        let ok = r#"{"id":"x","result":{"type":"pane_graphics_info","cell_width_px":17,"cell_height_px":35,"pane_visible":true}}"#;
        assert_eq!(parse_info(ok), Some(RETINA));
        let disabled = r#"{"id":"x","error":{"code":"feature_disabled","message":"off"}}"#;
        assert_eq!(parse_info(disabled), None);
        let unknown = r#"{"id":"x","result":{"type":"pane_graphics_info","cell_width_px":0,"cell_height_px":35}}"#;
        assert_eq!(parse_info(unknown), None);
        assert_eq!(parse_info("not json"), None);
    }

    #[test]
    fn a_wide_image_fills_the_width_and_is_centered_vertically() {
        // 117×24 cells of 17×35 px = 1989×840 px; a 1280×769 image is
        // height-bound: 840 px tall → 1398 px wide → 83 cells.
        let plan = plan(area(117, 24), RETINA, (1280, 769), FRAME_MAX_BYTES).unwrap();
        assert_eq!(plan.placement.rows, 24);
        assert_eq!(plan.placement.cols, 83);
        assert_eq!(plan.placement.col, (117 - 83) / 2);
        assert_eq!(plan.placement.row, 1);
    }

    #[test]
    fn the_frame_matches_the_cells_aspect_so_the_image_is_not_stretched() {
        let plan = plan(area(117, 24), RETINA, (1280, 769), FRAME_MAX_BYTES).unwrap();
        let cells_aspect =
            f64::from(plan.placement.cols * 17) / f64::from(plan.placement.rows * 35);
        let canvas_aspect = f64::from(plan.canvas.0) / f64::from(plan.canvas.1);
        assert!((cells_aspect - canvas_aspect).abs() < 0.01);
        let image_aspect = f64::from(plan.image_size.0) / f64::from(plan.image_size.1);
        assert!((image_aspect - 1280.0 / 769.0).abs() < 0.01);
        assert!(plan.image_at.0 + plan.image_size.0 <= plan.canvas.0);
        assert!(plan.image_at.1 + plan.image_size.1 <= plan.canvas.1);
    }

    #[test]
    fn large_sources_are_sent_at_display_resolution_not_cell_resolution() {
        // A 2480×3508 page into 100×50 cells: 1700×1750 px available.
        let plan = plan(area(100, 50), RETINA, (2480, 3508), FRAME_MAX_BYTES).unwrap();
        assert_eq!(plan.image_size.1, 1750);
        assert!(plan.image_size.0 > 1200, "{plan:?}");
        assert_eq!(plan.canvas.1, 1750);
    }

    #[test]
    fn small_sources_keep_their_native_pixels_and_the_terminal_upscales() {
        // Width-bound: 1700 px wide → 1700 px tall → 49 of the 50 rows.
        let plan = plan(area(100, 50), RETINA, (64, 64), FRAME_MAX_BYTES).unwrap();
        assert_eq!(plan.image_size, (64, 64));
        assert_eq!((plan.placement.cols, plan.placement.rows), (100, 49));
    }

    #[test]
    fn frames_never_exceed_the_stream_limit() {
        let huge_cells = CellSize {
            width_px: 40,
            height_px: 80,
        };
        let plan = plan(area(400, 120), huge_cells, (8000, 4000), FRAME_MAX_BYTES).unwrap();
        assert!(u64::from(plan.canvas.0) * u64::from(plan.canvas.1) * 4 <= FRAME_MAX_BYTES);
    }

    #[test]
    fn empty_inputs_have_no_plan() {
        assert_eq!(plan(area(0, 10), RETINA, (10, 10), FRAME_MAX_BYTES), None);
        assert_eq!(plan(area(10, 10), RETINA, (0, 10), FRAME_MAX_BYTES), None);
    }

    #[test]
    fn compose_pads_transparently_around_the_scaled_image() {
        let plan = FramePlan {
            placement: area(2, 1),
            canvas: (6, 4),
            image_at: (1, 0),
            image_size: (4, 4),
        };
        let source = image::RgbaImage::from_pixel(8, 8, image::Rgba([200, 10, 10, 255]));
        let frame = compose(&plan, &source);
        assert_eq!(frame.dimensions(), (6, 4));
        assert_eq!(frame.get_pixel(0, 0)[3], 0);
        assert_eq!(frame.get_pixel(5, 3)[3], 0);
        assert_eq!(*frame.get_pixel(2, 2), image::Rgba([200, 10, 10, 255]));
    }

    #[test]
    fn frame_headers_carry_the_canvas_and_the_cell_placement() {
        let plan = plan(area(117, 24), RETINA, (1280, 769), FRAME_MAX_BYTES).unwrap();
        let header: serde_json::Value = serde_json::from_str(&frame_header(&plan, 42)).unwrap();
        assert_eq!(header["format"], "rgba");
        assert_eq!(header["data_length"], 42);
        assert_eq!(header["image_width"], plan.canvas.0);
        assert_eq!(header["placement"]["grid_cols"], plan.placement.cols);
        assert_eq!(header["placement"]["viewport_row"], 1);
    }
}
