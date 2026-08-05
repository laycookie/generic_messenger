use std::{
    collections::{HashMap, VecDeque},
    fmt,
    sync::Mutex,
    time::Instant,
};

use tracing::{Event, Subscriber, field::Visit};
use tracing_subscriber::{
    fmt::{
        FmtContext,
        format::{FormatEvent, FormatFields, Writer},
    },
    registry::LookupSpan,
};

const MAX_PENDING_FRAMES: usize = 512;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";

#[derive(Clone, Debug)]
enum FieldValue {
    U64(u64),
    I64(i64),
    Bool(bool),
    Text(String),
}

#[derive(Clone, Debug, Default)]
struct TimingFields {
    values: Vec<(&'static str, FieldValue)>,
}

impl TimingFields {
    fn insert(&mut self, name: &'static str, value: FieldValue) {
        if let Some((_, current)) = self.values.iter_mut().find(|(field, _)| *field == name) {
            *current = value;
        } else {
            self.values.push((name, value));
        }
    }

    fn u64(&self, name: &str) -> Option<u64> {
        self.values
            .iter()
            .find_map(|(field, value)| match (*field == name, value) {
                (true, FieldValue::U64(value)) => Some(*value),
                (true, FieldValue::I64(value)) => u64::try_from(*value).ok(),
                _ => None,
            })
    }

    fn bool(&self, name: &str) -> Option<bool> {
        self.values
            .iter()
            .find_map(|(field, value)| match (*field == name, value) {
                (true, FieldValue::Bool(value)) => Some(*value),
                _ => None,
            })
    }

    fn text(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .find_map(|(field, value)| match (*field == name, value) {
                (true, FieldValue::Text(value)) => Some(value.as_str()),
                _ => None,
            })
    }

    fn stage(&self) -> &str {
        self.text("stage").unwrap_or("video.event")
    }
}

impl Visit for TimingFields {
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.insert(field.name(), FieldValue::U64(value));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.insert(field.name(), FieldValue::I64(value));
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.insert(field.name(), FieldValue::Bool(value));
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.insert(field.name(), FieldValue::Text(value.to_string()));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.insert(field.name(), FieldValue::Text(value.to_owned()));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        self.insert(field.name(), FieldValue::Text(format!("{value:?}")));
    }
}

#[derive(Default)]
struct FrameReport {
    frame_id: u64,
    stages: Vec<TimingFields>,
}

impl FrameReport {
    fn record(&mut self, fields: TimingFields) {
        let stage = fields.stage();
        if let Some(current) = self
            .stages
            .iter_mut()
            .find(|current| current.stage() == stage)
        {
            *current = fields;
        } else {
            self.stages.push(fields);
        }
    }

    fn field(&self, name: &str) -> Option<u64> {
        self.stages.iter().rev().find_map(|stage| stage.u64(name))
    }

    fn text(&self, name: &str) -> Option<&str> {
        self.stages.iter().rev().find_map(|stage| stage.text(name))
    }

    fn final_stage(&self) -> Option<&TimingFields> {
        self.stages
            .iter()
            .find(|stage| stage.stage() == "outgoing.complete")
    }
}

#[derive(Default)]
struct ReportState {
    frames: HashMap<u64, FrameReport>,
    insertion_order: VecDeque<u64>,
    stale_replacements: u64,
}

impl ReportState {
    fn record(&mut self, frame_id: u64, fields: TimingFields) {
        if !self.frames.contains_key(&frame_id) {
            self.insertion_order.push_back(frame_id);
        }
        let report = self.frames.entry(frame_id).or_insert_with(|| FrameReport {
            frame_id,
            stages: Vec::with_capacity(12),
        });
        report.record(fields);
        self.evict_old_frames();
    }

    fn evict_old_frames(&mut self) {
        while self.frames.len() > MAX_PENDING_FRAMES {
            let Some(frame_id) = self.insertion_order.pop_front() else {
                break;
            };
            self.frames.remove(&frame_id);
        }
        while self
            .insertion_order
            .front()
            .is_some_and(|frame_id| !self.frames.contains_key(frame_id))
        {
            self.insertion_order.pop_front();
        }
    }
}

/// Human-focused formatter for structured `video_timing` events.
///
/// Events remain structured at call sites. Formatter aggregates stages by
/// frame and prints one report only after Discord finishes sending frame.
pub(crate) struct VideoTimingFormatter {
    started_at: Instant,
    state: Mutex<ReportState>,
}

impl VideoTimingFormatter {
    pub(crate) fn new() -> Self {
        Self {
            started_at: Instant::now(),
            state: Mutex::new(ReportState::default()),
        }
    }
}

impl<S, N> FormatEvent<S, N> for VideoTimingFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _context: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut fields = TimingFields::default();
        event.record(&mut fields);

        let Some(frame_id) = correlated_frame_id(&fields) else {
            return render_single_stage(
                &fields,
                self.started_at.elapsed().as_secs_f64(),
                writer.has_ansi_escapes(),
                &mut writer,
            );
        };

        let is_stale_replacement =
            fields.stage() == "channel.publish" && fields.bool("stale_replaced") == Some(true);
        let is_complete = fields.stage() == "outgoing.complete"
            && (fields.bool("emitted") == Some(true) || fields.bool("failed") == Some(true));

        let (report, stale_replacements) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if is_stale_replacement {
                state.stale_replacements = state.stale_replacements.saturating_add(1);
            }
            state.record(frame_id, fields);
            if is_complete {
                let report = state.frames.remove(&frame_id);
                let stale_replacements = std::mem::take(&mut state.stale_replacements);
                (report, stale_replacements)
            } else {
                return Ok(());
            }
        };

        if let Some(mut report) = report {
            report.stages.sort_by_key(|stage| stage_rank(stage.stage()));
            render_report(
                &report,
                stale_replacements,
                self.started_at.elapsed().as_secs_f64(),
                writer.has_ansi_escapes(),
                &mut writer,
            )?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Component {
    field: &'static str,
    label: &'static str,
    actionable: bool,
}

const SCREEN_COMPONENTS: &[Component] = &[
    Component {
        field: "wait_us",
        label: "frame wait (expected cadence)",
        actionable: false,
    },
    Component {
        field: "normalize_us",
        label: "normalize + downscale",
        actionable: true,
    },
];
const WEBCAM_COMPONENTS: &[Component] = &[
    Component {
        field: "wait_us",
        label: "device frame wait",
        actionable: false,
    },
    Component {
        field: "decode_us",
        label: "camera format decode",
        actionable: true,
    },
];
const CAPTURE_COMPONENTS: &[Component] = &[
    Component {
        field: "capture_us",
        label: "capture + normalize envelope",
        actionable: false,
    },
    Component {
        field: "publish_us",
        label: "publish to latest-frame queue",
        actionable: true,
    },
];
const SCALE_COMPONENTS: &[Component] = &[Component {
    field: "elapsed_us",
    label: "software pixel scaling",
    actionable: true,
}];
const CHANNEL_COMPONENTS: &[Component] = &[Component {
    field: "lock_wait_us",
    label: "channel lock wait",
    actionable: true,
}];
const INPUT_COMPONENTS: &[Component] = &[
    Component {
        field: "pacing_wait_us",
        label: "frame pacing wait",
        actionable: false,
    },
    Component {
        field: "input_wait_us",
        label: "input queue wait",
        actionable: false,
    },
];
const H264_COMPONENTS: &[Component] = &[
    Component {
        field: "rgb_to_yuv_us",
        label: "RGB → YUV conversion",
        actionable: true,
    },
    Component {
        field: "codec_us",
        label: "H264 codec",
        actionable: true,
    },
    Component {
        field: "output_copy_us",
        label: "encoded output copy",
        actionable: true,
    },
];
const AV1_COMPONENTS: &[Component] = &[
    Component {
        field: "rgb_to_yuv_us",
        label: "RGB → YUV conversion",
        actionable: true,
    },
    Component {
        field: "codec_us",
        label: "AV1 codec",
        actionable: true,
    },
];
const H264_DECODE_COMPONENTS: &[Component] = &[
    Component {
        field: "codec_us",
        label: "H264 codec decode",
        actionable: true,
    },
    Component {
        field: "rgba_us",
        label: "YUV → RGBA conversion",
        actionable: true,
    },
];
const INCOMING_COMPONENTS: &[Component] = &[
    Component {
        field: "packet_assemble_us",
        label: "RTP frame assembly CPU",
        actionable: true,
    },
    Component {
        field: "assembly_age_us",
        label: "first → last RTP packet",
        actionable: false,
    },
    Component {
        field: "dave_lock_wait_us",
        label: "DAVE lock wait",
        actionable: true,
    },
    Component {
        field: "dave_decrypt_us",
        label: "DAVE frame decryption",
        actionable: true,
    },
    Component {
        field: "decode_us",
        label: "decode envelope",
        actionable: true,
    },
];
const PREVIEW_COMPONENTS: &[Component] = &[Component {
    field: "scale_us",
    label: "preview frame scaling",
    actionable: true,
}];
const WORKER_COMPONENTS: &[Component] = &[
    Component {
        field: "worker_queue_us",
        label: "blocking-worker queue",
        actionable: true,
    },
    Component {
        field: "elapsed_us",
        label: "worker execution envelope",
        actionable: false,
    },
];
const TRANSPORT_COMPONENTS: &[Component] = &[
    Component {
        field: "dave_lock_wait_us",
        label: "DAVE lock wait",
        actionable: true,
    },
    Component {
        field: "dave_encrypt_us",
        label: "DAVE frame encryption",
        actionable: true,
    },
    Component {
        field: "packetize_us",
        label: "codec packetization",
        actionable: true,
    },
    Component {
        field: "history_us",
        label: "RTX history copy",
        actionable: true,
    },
    Component {
        field: "aead_us",
        label: "RTP AEAD encryption",
        actionable: true,
    },
    Component {
        field: "udp_send_us",
        label: "UDP send syscalls",
        actionable: true,
    },
    Component {
        field: "pacing_us",
        label: "packet pacing wait",
        actionable: false,
    },
    Component {
        field: "sender_report_us",
        label: "RTCP sender report",
        actionable: true,
    },
];
const COMPLETE_COMPONENTS: &[Component] = &[
    Component {
        field: "signaling_us",
        label: "Discord signaling",
        actionable: true,
    },
    Component {
        field: "announce_settle_us",
        label: "first-frame announce settle",
        actionable: false,
    },
    Component {
        field: "job_us",
        label: "encode + transport job envelope",
        actionable: false,
    },
];
const NO_COMPONENTS: &[Component] = &[];

fn components(stage: &str) -> &'static [Component] {
    match stage {
        "screen.capture" => SCREEN_COMPONENTS,
        "webcam.capture" => WEBCAM_COMPONENTS,
        "capture.bounded" => CAPTURE_COMPONENTS,
        "frame.scale" => SCALE_COMPONENTS,
        "channel.publish" | "channel.consume" | "channel.try_consume" => CHANNEL_COMPONENTS,
        "outgoing.input_wait" => INPUT_COMPONENTS,
        "h264.encode" => H264_COMPONENTS,
        "av1.encode" => AV1_COMPONENTS,
        "h264.decode" => H264_DECODE_COMPONENTS,
        "incoming.receive" => INCOMING_COMPONENTS,
        "ui.preview_select" => PREVIEW_COMPONENTS,
        "outgoing.encode_worker" => WORKER_COMPONENTS,
        "outgoing.transport" => TRANSPORT_COMPONENTS,
        "outgoing.complete" => COMPLETE_COMPONENTS,
        _ => NO_COMPONENTS,
    }
}

fn stage_rank(stage: &str) -> u8 {
    match stage {
        "webcam.capture" | "screen.capture" => 10,
        "frame.scale" => 11,
        "capture.bounded" => 12,
        "channel.publish" => 20,
        "channel.consume" | "channel.try_consume" => 21,
        "outgoing.input_wait" => 30,
        "outgoing.accept" => 31,
        "h264.encode" | "av1.encode" => 40,
        "outgoing.encode_worker" => 41,
        "outgoing.transport" => 50,
        "outgoing.complete" => 60,
        _ => 55,
    }
}

fn stage_group(stage: &str) -> &'static str {
    match stage_rank(stage) {
        0..=19 => "CAPTURE",
        20..=29 => "LATEST-FRAME QUEUE",
        30..=39 => "DISCORD SCHEDULING",
        40..=49 => "ENCODE",
        50..=59 => "TRANSPORT",
        _ => "FINAL",
    }
}

fn stage_total_us(fields: &TimingFields) -> Option<u64> {
    match fields.stage() {
        "outgoing.complete" => fields.u64("pipeline_elapsed_us"),
        "ui.preview_select" => fields.u64("scale_us"),
        "outgoing.encode_worker" => fields
            .u64("worker_queue_us")
            .zip(fields.u64("elapsed_us"))
            .map(|(queue, work)| queue.saturating_add(work)),
        _ => fields.u64("elapsed_us").or_else(|| fields.u64("total_us")),
    }
}

fn stage_label(stage: &str) -> &str {
    match stage {
        "screen.capture" => "screen capture + normalize",
        "webcam.capture" => "webcam capture + decode",
        "capture.bounded" => "capture envelope → queue",
        "frame.scale" => "software frame scale",
        "channel.publish" => "publish newest frame",
        "channel.consume" => "consume newest frame",
        "channel.try_consume" => "consume preview frame",
        "outgoing.input_wait" => "Discord frame pacing/input",
        "outgoing.accept" => "Discord accept frame",
        "h264.encode" => "H264 encode",
        "av1.encode" => "AV1 encode",
        "h264.decode" => "H264 decode",
        "incoming.receive" => "incoming Discord frame",
        "ui.preview_select" => "UI preview scale",
        "ui.texture_handle" => "UI texture upload",
        "capture.open" => "capture device open",
        "outgoing.encode_worker" => "encode worker envelope",
        "outgoing.transport" => "encrypt + packetize + UDP",
        "outgoing.complete" => "capture → Discord send",
        other => other,
    }
}

fn correlated_frame_id(fields: &TimingFields) -> Option<u64> {
    if !matches!(
        fields.stage(),
        "screen.capture"
            | "webcam.capture"
            | "capture.bounded"
            | "frame.scale"
            | "channel.publish"
            | "channel.consume"
            | "channel.try_consume"
            | "outgoing.input_wait"
            | "outgoing.accept"
            | "h264.encode"
            | "av1.encode"
            | "outgoing.encode_worker"
            | "outgoing.transport"
            | "outgoing.complete"
    ) {
        return None;
    }

    fields
        .u64("emitted_frame_id")
        .filter(|frame_id| *frame_id != 0)
        .or_else(|| fields.u64("frame_id").filter(|frame_id| *frame_id != 0))
}

fn hottest_component(report: &FrameReport) -> Option<(&'static str, u64)> {
    report
        .stages
        .iter()
        .flat_map(|stage| {
            components(stage.stage())
                .iter()
                .filter(|component| component.actionable)
                .filter_map(|component| {
                    stage
                        .u64(component.field)
                        .map(|duration| (component.label, duration))
                })
        })
        .max_by_key(|(_, duration)| *duration)
}

fn render_report(
    report: &FrameReport,
    stale_replacements: u64,
    uptime_seconds: f64,
    ansi: bool,
    output: &mut impl fmt::Write,
) -> fmt::Result {
    let final_stage = report.final_stage();
    let pipeline_us = final_stage
        .and_then(|stage| stage.u64("pipeline_elapsed_us"))
        .or_else(|| report.field("pipeline_elapsed_us"))
        .unwrap_or_default();
    let budget_us = final_stage
        .and_then(|stage| stage.u64("frame_budget_us"))
        .unwrap_or_default();
    let kind = final_stage
        .and_then(|stage| stage.text("kind"))
        .or_else(|| report.text("kind"))
        .unwrap_or("Video");
    let ratio = (budget_us > 0).then(|| pipeline_us as f64 / budget_us as f64);
    let over_budget = ratio.is_some_and(|ratio| ratio > 1.0);

    let style = Styles::new(ansi);
    writeln!(output)?;
    writeln!(
        output,
        "{}╭─ {}{}VIDEO PIPELINE{} · {}{}{} · frame #{} {}+{uptime_seconds:.3}s{}",
        style.dim,
        style.bold,
        style.cyan,
        style.reset,
        style.bold,
        kind.to_uppercase(),
        style.reset,
        report.frame_id,
        style.dim,
        style.reset,
    )?;
    write!(
        output,
        "{}│{}  {}END TO END{:>22.3} ms{}",
        style.dim,
        style.reset,
        style.bold,
        micros_to_ms(pipeline_us),
        style.reset,
    )?;
    if let Some(ratio) = ratio {
        let budget_color = if over_budget { style.red } else { style.green };
        write!(
            output,
            "   {budget_color}{ratio:.1}× {} BUDGET{}",
            if over_budget { "OVER" } else { "OF" },
            style.reset,
        )?;
    }
    writeln!(output)?;

    if let Some((label, duration_us)) = hottest_component(report) {
        let percent = percentage(duration_us, pipeline_us);
        writeln!(
            output,
            "{}│{}  {}BOTTLENECK{}  {:<25} {}{:>9.3} ms{}  ({percent:>5.1}% of pipeline)",
            style.dim,
            style.reset,
            style.red,
            style.reset,
            label,
            style.red,
            micros_to_ms(duration_us),
            style.reset,
        )?;
    }

    if stale_replacements > 0 {
        writeln!(
            output,
            "{}│{}  {}DROPPED{}     {stale_replacements} stale frame{} replaced in latest-frame queue",
            style.dim,
            style.reset,
            style.yellow,
            style.reset,
            if stale_replacements == 1 { "" } else { "s" },
        )?;
    }

    let mut group = "";
    for stage in &report.stages {
        if stage_total_us(stage).is_none() {
            continue;
        }
        let next_group = stage_group(stage.stage());
        if group != next_group {
            group = next_group;
            writeln!(
                output,
                "{}├─ {}{}{}{}",
                style.dim, style.cyan, group, style.reset, style.dim
            )?;
        }
        render_stage(stage, pipeline_us, &style, output)?;
    }
    writeln!(
        output,
        "{}╰────────────────────────────────────────────────────────────────────{}",
        style.dim, style.reset
    )
}

fn render_single_stage(
    fields: &TimingFields,
    uptime_seconds: f64,
    ansi: bool,
    output: &mut impl fmt::Write,
) -> fmt::Result {
    let style = Styles::new(ansi);
    let duration = stage_total_us(fields).unwrap_or_default();
    writeln!(
        output,
        "{}VIDEO PERF{} · {:<30} {}{:>9.3} ms{}  {}+{uptime_seconds:.3}s{}",
        style.cyan,
        style.reset,
        stage_label(fields.stage()),
        style.bold,
        micros_to_ms(duration),
        style.reset,
        style.dim,
        style.reset,
    )?;
    render_component_rows(fields, duration, "              ", &style, output)
}

fn render_stage(
    fields: &TimingFields,
    pipeline_us: u64,
    style: &Styles<'_>,
    output: &mut impl fmt::Write,
) -> fmt::Result {
    let total_us = stage_total_us(fields).unwrap_or_default();
    writeln!(
        output,
        "{}│{}  {}{:<34}{} {:>9.3} ms",
        style.dim,
        style.reset,
        style.bold,
        stage_label(fields.stage()),
        style.reset,
        micros_to_ms(total_us),
    )?;

    render_component_rows(fields, total_us, "│    ", style, output)?;

    render_stage_details(fields, pipeline_us, style, output)
}

fn render_component_rows(
    fields: &TimingFields,
    total_us: u64,
    prefix: &str,
    style: &Styles<'_>,
    output: &mut impl fmt::Write,
) -> fmt::Result {
    let hottest = components(fields.stage())
        .iter()
        .filter_map(|component| {
            fields
                .u64(component.field)
                .map(|duration| (component.field, duration))
        })
        .max_by_key(|(_, duration)| *duration)
        .map(|(field, _)| field);
    for component in components(fields.stage()) {
        let Some(duration_us) = fields.u64(component.field) else {
            continue;
        };
        let denominator = total_us.max(1);
        let percent = percentage(duration_us, denominator);
        let is_hot = component.actionable
            && hottest == Some(component.field)
            && duration_us >= 1_000
            && percent >= 50.0;
        let color = if is_hot { style.red } else { style.dim };
        writeln!(
            output,
            "{}{prefix}├─{} {:<32} {color}{:>9.3} ms{}  {:>5.1}%{}",
            style.dim,
            style.reset,
            component.label,
            micros_to_ms(duration_us),
            style.reset,
            percent,
            if is_hot { "  ← HOT" } else { "" },
        )?;
    }
    Ok(())
}

fn render_stage_details(
    fields: &TimingFields,
    pipeline_us: u64,
    style: &Styles<'_>,
    output: &mut impl fmt::Write,
) -> fmt::Result {
    match fields.stage() {
        "screen.capture" | "webcam.capture" => {
            let source_width = fields.u64("source_width").unwrap_or_default();
            let source_height = fields.u64("source_height").unwrap_or_default();
            let output_width = fields.u64("output_width").unwrap_or_default();
            let output_height = fields.u64("output_height").unwrap_or_default();
            let format = fields.text("source_format").unwrap_or("pixels");
            writeln!(
                output,
                "{}│{}    ╰─ {}×{} {format} ({}) → {}×{} ({})",
                style.dim,
                style.reset,
                source_width,
                source_height,
                bytes(fields.u64("source_bytes").unwrap_or_default()),
                output_width,
                output_height,
                bytes(fields.u64("output_bytes").unwrap_or_default()),
            )?;
        }
        "channel.consume" | "channel.try_consume" => {
            if let Some(queue_age_us) = fields.u64("queue_age_us") {
                writeln!(
                    output,
                    "{}│{}    ╰─ frame waited {:>9.3} ms in latest-frame queue",
                    style.dim,
                    style.reset,
                    micros_to_ms(queue_age_us),
                )?;
            }
        }
        "h264.encode" | "av1.encode" => {
            writeln!(
                output,
                "{}│{}    ╰─ {}×{} · {} raw → {} encoded",
                style.dim,
                style.reset,
                fields.u64("width").unwrap_or_default(),
                fields.u64("height").unwrap_or_default(),
                bytes(fields.u64("input_bytes").unwrap_or_default()),
                bytes(fields.u64("output_bytes").unwrap_or_default()),
            )?;
        }
        "outgoing.transport" => {
            writeln!(
                output,
                "{}│{}    ╰─ {} RTP packet{} · {} on wire",
                style.dim,
                style.reset,
                fields.u64("packets_sent").unwrap_or_default(),
                if fields.u64("packets_sent") == Some(1) {
                    ""
                } else {
                    "s"
                },
                bytes(fields.u64("wire_bytes").unwrap_or_default()),
            )?;
        }
        "outgoing.complete" => {
            let discord_us = fields.u64("discord_us").unwrap_or_default();
            writeln!(
                output,
                "{}│{}    ╰─ Discord-side job {:>9.3} ms · pipeline {:>9.3} ms",
                style.dim,
                style.reset,
                micros_to_ms(discord_us),
                micros_to_ms(pipeline_us),
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn micros_to_ms(micros: u64) -> f64 {
    micros as f64 / 1_000.0
}

fn percentage(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 / total as f64 * 100.0
    }
}

fn bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    match bytes {
        0..=1023 => format!("{bytes} B"),
        1024..=1_048_575 => format!("{:.2} KiB", bytes as f64 / KIB),
        _ => format!("{:.2} MiB", bytes as f64 / MIB),
    }
}

struct Styles<'a> {
    reset: &'a str,
    bold: &'a str,
    dim: &'a str,
    cyan: &'a str,
    green: &'a str,
    yellow: &'a str,
    red: &'a str,
}

impl Styles<'static> {
    fn new(ansi: bool) -> Self {
        if ansi {
            Self {
                reset: RESET,
                bold: BOLD,
                dim: DIM,
                cyan: CYAN,
                green: GREEN,
                yellow: YELLOW,
                red: RED,
            }
        } else {
            Self {
                reset: "",
                bold: "",
                dim: "",
                cyan: "",
                green: "",
                yellow: "",
                red: "",
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(stage: &'static str, values: &[(&'static str, u64)]) -> TimingFields {
        let mut fields = TimingFields::default();
        fields.insert("stage", FieldValue::Text(stage.to_owned()));
        fields.insert("frame_id", FieldValue::U64(42));
        for (name, value) in values {
            fields.insert(name, FieldValue::U64(*value));
        }
        fields
    }

    fn sample_report() -> FrameReport {
        let mut report = FrameReport {
            frame_id: 42,
            stages: Vec::new(),
        };
        report.record(fields(
            "screen.capture",
            &[
                ("elapsed_us", 20_281),
                ("wait_us", 19_198),
                ("normalize_us", 1_083),
            ],
        ));
        report.record(fields(
            "h264.encode",
            &[
                ("total_us", 392_365),
                ("rgb_to_yuv_us", 1_026),
                ("codec_us", 391_333),
                ("output_copy_us", 4),
            ],
        ));
        let mut complete = fields(
            "outgoing.complete",
            &[
                ("pipeline_elapsed_us", 412_085),
                ("frame_budget_us", 33_333),
                ("discord_us", 392_500),
                ("job_us", 392_490),
            ],
        );
        complete.insert("kind", FieldValue::Text("Screen".to_owned()));
        complete.insert("emitted", FieldValue::Bool(true));
        report.record(complete);
        report.stages.sort_by_key(|stage| stage_rank(stage.stage()));
        report
    }

    #[test]
    fn report_uses_milliseconds_and_highlights_codec_bottleneck() {
        let mut output = String::new();

        render_report(&sample_report(), 11, 25.2, false, &mut output).unwrap();

        assert!(output.contains("END TO END               412.085 ms"));
        assert!(output.contains("BOTTLENECK  H264 codec"));
        assert!(output.contains("391.333 ms"));
        assert!(output.contains("11 stale frames replaced"));
        assert!(!output.contains("elapsed_us"));
    }

    #[test]
    fn ansi_is_emitted_only_when_enabled() {
        let mut plain = String::new();
        let mut colored = String::new();

        render_report(&sample_report(), 0, 1.0, false, &mut plain).unwrap();
        render_report(&sample_report(), 0, 1.0, true, &mut colored).unwrap();

        assert!(!plain.contains("\x1b["));
        assert!(colored.contains("\x1b[31m"));
    }

    #[test]
    fn state_evicts_old_incomplete_frames() {
        let mut state = ReportState::default();
        for frame_id in 1..=MAX_PENDING_FRAMES as u64 + 20 {
            state.record(frame_id, fields("channel.publish", &[]));
        }

        assert_eq!(state.frames.len(), MAX_PENDING_FRAMES);
        assert!(!state.frames.contains_key(&1));
    }

    #[test]
    fn inbound_decoder_ids_are_not_correlated_with_outbound_frames() {
        let inbound = fields("h264.decode", &[("frame_id", 42)]);
        assert_eq!(correlated_frame_id(&inbound), None);

        let mut av1 = fields("av1.encode", &[("frame_id", 44), ("emitted_frame_id", 42)]);
        av1.insert("emitted", FieldValue::Bool(true));
        assert_eq!(correlated_frame_id(&av1), Some(42));
    }
}
