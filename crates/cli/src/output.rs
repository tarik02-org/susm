use std::{
    error::Error,
    fmt::Display,
    io::{self, Write},
    path::Path,
};

use anstream::{AutoStream, ColorChoice};
use anstyle::{AnsiColor, Style};
use clap::ValueEnum;
use comfy_table::{Attribute, Cell, Color, ContentArrangement, Table, presets::NOTHING};
use susm_protocol::{
    control::{Execution, ReadLogsResponse, Workload},
    host::HostStatus,
};

use crate::installer::InstalledVersion;

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum ColorMode {
    #[default]
    Auto,
    Always,
    Never,
}

impl From<ColorMode> for ColorChoice {
    fn from(value: ColorMode) -> Self {
        match value {
            ColorMode::Auto => Self::Auto,
            ColorMode::Always => Self::Always,
            ColorMode::Never => Self::Never,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LogOptions {
    pub timestamps: bool,
    pub prefix: bool,
}

pub struct HumanOutput {
    stdout: AutoStream<io::Stdout>,
    stderr: AutoStream<io::Stderr>,
    stdout_colors: bool,
    stderr_colors: bool,
    stdout_line_start: bool,
    stderr_line_start: bool,
}

impl HumanOutput {
    pub fn new(mode: ColorMode) -> Self {
        let stdout = AutoStream::new(io::stdout(), mode.into());
        let stderr = AutoStream::new(io::stderr(), mode.into());
        let stdout_colors = stdout.current_choice() != ColorChoice::Never;
        let stderr_colors = stderr.current_choice() != ColorChoice::Never;
        Self {
            stdout,
            stderr,
            stdout_colors,
            stderr_colors,
            stdout_line_start: true,
            stderr_line_start: true,
        }
    }

    pub fn error(&mut self, error: &(dyn Error + 'static)) -> io::Result<()> {
        write_label(
            &mut self.stderr,
            self.stderr_colors,
            AnsiColor::Red.on_default().bold(),
            "error:",
        )?;
        if let Some(status) = error.downcast_ref::<tonic::Status>() {
            writeln!(self.stderr, " {}", status.message())
        } else {
            writeln!(self.stderr, " {error}")
        }
    }

    pub fn path(&mut self, path: &Path) -> io::Result<()> {
        writeln!(self.stdout, "{}", path.display())
    }

    pub fn installed(&mut self, version: &InstalledVersion, controller_pid: u32) -> io::Result<()> {
        self.success(&format!("Installed SUSM {}", version.version))?;
        self.details([
            ("Identity", version.identity.as_str()),
            ("Path", &version.path.display().to_string()),
            ("Controller PID", &controller_pid.to_string()),
        ])
    }

    pub fn uninstalled(&mut self) -> io::Result<()> {
        self.success("Removed the per-user SUSM installation")
    }

    pub fn selected(&mut self, version: &InstalledVersion) -> io::Result<()> {
        self.success(&format!("Selected SUSM {}", version.version))?;
        self.details([
            ("Identity", version.identity.as_str()),
            ("Path", &version.path.display().to_string()),
        ])
    }

    pub fn garbage_collected(&mut self, removed: usize) -> io::Result<()> {
        self.success(&format!(
            "Removed {removed} unused version or staging entries"
        ))
    }

    pub fn versions(&mut self, versions: &[InstalledVersion]) -> io::Result<()> {
        if versions.is_empty() {
            return self.empty("No installed versions");
        }
        let mut table = self.table(["VERSION", "IDENTITY", "STATUS", "PINS", "PATH"]);
        for version in versions {
            table.add_row([
                Cell::new(&version.version),
                Cell::new(shorten(&version.identity, 12)),
                self.status_cell(if version.current {
                    "current"
                } else {
                    "installed"
                }),
                Cell::new(version.pin_count),
                Cell::new(version.path.display()),
            ]);
        }
        self.write_table(&table)
    }

    pub fn controller_status(&mut self, status: &HostStatus) -> io::Result<()> {
        let state = if status.controller_running {
            "running"
        } else if status.registered {
            "recovering"
        } else {
            "not registered"
        };
        let mut table = self.detail_table();
        table.add_row([self.detail_key("State"), self.status_cell(state)]);
        table.add_row([
            self.detail_key("Registered"),
            self.bool_cell(status.registered),
        ]);
        if !status.manager_session_id.is_empty() {
            table.add_row([
                self.detail_key("Manager session"),
                Cell::new(&status.manager_session_id),
            ]);
        }
        if status.controller_process_id != 0 {
            table.add_row([
                self.detail_key("Process ID"),
                Cell::new(status.controller_process_id),
            ]);
        }
        if !status.message.is_empty() {
            table.add_row([self.detail_key("Message"), Cell::new(&status.message)]);
        }
        self.write_table(&table)
    }

    pub fn controller_restart(&mut self, manager_session_id: &str) -> io::Result<()> {
        self.success("Controller restart requested")?;
        self.details([("Manager session", manager_session_id)])
    }

    pub fn reload(&mut self, changed: bool, generation: &str) -> io::Result<()> {
        if changed {
            self.success("Configuration reloaded")?;
        } else {
            self.notice("Configuration unchanged")?;
        }
        self.details([("Generation", generation)])
    }

    pub fn reload_diagnostics<'a>(
        &mut self,
        diagnostics: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> io::Result<()> {
        let mut table = self.table(["FILE", "ERROR"]);
        for (path, message) in diagnostics {
            table.add_row([
                Cell::new(path),
                color_cell(message, Color::Red, self.stdout_colors),
            ]);
        }
        self.write_table(&table)
    }

    pub fn workloads(&mut self, workloads: &[Workload]) -> io::Result<()> {
        if workloads.is_empty() {
            return self.empty("No workloads");
        }
        let mut table = self.table([
            "NAME", "KIND", "STATE", "ENABLED", "PID", "ATTEMPT", "NOTES",
        ]);
        for workload in workloads {
            table.add_row([
                Cell::new(&workload.workload_id),
                color_cell(&workload.kind, Color::Cyan, self.stdout_colors),
                self.status_cell(&workload.state),
                self.bool_cell(workload.enabled),
                Cell::new(optional_number(workload.workload_process_id)),
                Cell::new(optional_number(workload.attempt)),
                self.notes_cell(workload),
            ]);
        }
        self.write_table(&table)
    }

    pub fn workload(&mut self, workload: &Workload) -> io::Result<()> {
        let mut table = self.detail_table();
        table.add_row([self.detail_key("Name"), Cell::new(&workload.workload_id)]);
        table.add_row([
            self.detail_key("Kind"),
            color_cell(&workload.kind, Color::Cyan, self.stdout_colors),
        ]);
        table.add_row([self.detail_key("State"), self.status_cell(&workload.state)]);
        table.add_row([self.detail_key("Enabled"), self.bool_cell(workload.enabled)]);
        if !workload.execution_id.is_empty() {
            table.add_row([
                self.detail_key("Execution"),
                Cell::new(&workload.execution_id),
            ]);
        }
        if workload.workload_process_id != 0 {
            table.add_row([
                self.detail_key("Process ID"),
                Cell::new(workload.workload_process_id),
            ]);
        }
        if workload.supervisor_process_id != 0 {
            table.add_row([
                self.detail_key("Supervisor PID"),
                Cell::new(workload.supervisor_process_id),
            ]);
        }
        if workload.attempt != 0 {
            table.add_row([self.detail_key("Attempt"), Cell::new(workload.attempt)]);
        }
        if !workload.last_outcome.is_empty() {
            table.add_row([
                self.detail_key("Last outcome"),
                self.status_cell(&workload.last_outcome),
            ]);
        }
        let notes = workload_notes(workload);
        if !notes.is_empty() {
            table.add_row([
                self.detail_key("Notes"),
                color_cell(notes.join(", "), Color::Yellow, self.stdout_colors),
            ]);
        }
        if !workload.error.is_empty() {
            table.add_row([
                self.detail_key("Error"),
                color_cell(&workload.error, Color::Red, self.stdout_colors),
            ]);
        }
        self.write_table(&table)
    }

    pub fn mutation(&mut self, changed: bool, workload: &Workload) -> io::Result<()> {
        if changed {
            self.success(&format!("Updated {}", workload.workload_id))?;
        } else {
            self.notice(&format!("{} unchanged", workload.workload_id))?;
        }
        let mut table = self.detail_table();
        table.add_row([self.detail_key("State"), self.status_cell(&workload.state)]);
        if workload.workload_process_id != 0 {
            table.add_row([
                self.detail_key("Process ID"),
                Cell::new(workload.workload_process_id),
            ]);
        }
        if !workload.error.is_empty() {
            table.add_row([
                self.detail_key("Error"),
                color_cell(&workload.error, Color::Red, self.stdout_colors),
            ]);
        }
        self.write_table(&table)
    }

    pub fn executions(&mut self, executions: &[Execution]) -> io::Result<()> {
        if executions.is_empty() {
            return self.empty("No executions");
        }
        let mut table = self.table([
            "EXECUTION",
            "STATE",
            "STARTED",
            "PID",
            "ATTEMPT",
            "EXIT",
            "ERROR",
        ]);
        for execution in executions {
            table.add_row([
                Cell::new(&execution.execution_id),
                self.status_cell(&execution.state),
                Cell::new(format_timestamp(execution.started_unix_ms)),
                Cell::new(optional_number(execution.workload_process_id)),
                Cell::new(optional_number(execution.attempt)),
                Cell::new(
                    execution
                        .exit_code
                        .map_or_else(|| "-".to_owned(), |value| value.to_string()),
                ),
                color_cell(&execution.error, Color::Red, self.stdout_colors),
            ]);
        }
        self.write_table(&table)
    }

    pub fn execution(&mut self, execution: &Execution) -> io::Result<()> {
        let mut table = self.detail_table();
        table.add_row([
            self.detail_key("Execution"),
            Cell::new(&execution.execution_id),
        ]);
        table.add_row([
            self.detail_key("Workload"),
            Cell::new(&execution.workload_id),
        ]);
        table.add_row([self.detail_key("State"), self.status_cell(&execution.state)]);
        table.add_row([
            self.detail_key("Started"),
            Cell::new(format_timestamp(execution.started_unix_ms)),
        ]);
        if execution.ended_unix_ms != 0 {
            table.add_row([
                self.detail_key("Ended"),
                Cell::new(format_timestamp(execution.ended_unix_ms)),
            ]);
        }
        if execution.supervisor_process_id != 0 {
            table.add_row([
                self.detail_key("Supervisor PID"),
                Cell::new(execution.supervisor_process_id),
            ]);
        }
        if execution.workload_process_id != 0 {
            table.add_row([
                self.detail_key("Process ID"),
                Cell::new(execution.workload_process_id),
            ]);
        }
        if execution.attempt != 0 {
            table.add_row([self.detail_key("Attempt"), Cell::new(execution.attempt)]);
        }
        if let Some(exit_code) = execution.exit_code {
            table.add_row([self.detail_key("Exit code"), Cell::new(exit_code)]);
        }
        if !execution.error.is_empty() {
            table.add_row([
                self.detail_key("Error"),
                color_cell(&execution.error, Color::Red, self.stdout_colors),
            ]);
        }
        self.write_table(&table)
    }

    pub fn log_record(&mut self, record: &ReadLogsResponse, options: LogOptions) -> io::Result<()> {
        if !record.gap.is_empty() {
            return self.log_gap(&record.gap);
        }
        let stderr = record.stream == "stderr";
        if stderr {
            let mut writer = io::stderr().lock();
            write_log_message(
                &mut writer,
                &mut self.stderr_line_start,
                self.stderr_colors,
                record,
                options,
            )
        } else {
            let mut writer = io::stdout().lock();
            write_log_message(
                &mut writer,
                &mut self.stdout_line_start,
                self.stdout_colors,
                record,
                options,
            )
        }
    }

    fn log_gap(&mut self, gap: &str) -> io::Result<()> {
        if !self.stderr_line_start {
            writeln!(self.stderr)?;
            self.stderr_line_start = true;
        }
        write_label(
            &mut self.stderr,
            self.stderr_colors,
            AnsiColor::Yellow.on_default().bold(),
            "warning:",
        )?;
        writeln!(self.stderr, " log gap: {gap}")
    }

    fn success(&mut self, message: &str) -> io::Result<()> {
        write_label(
            &mut self.stdout,
            self.stdout_colors,
            AnsiColor::Green.on_default().bold(),
            "✓",
        )?;
        writeln!(self.stdout, " {message}")
    }

    fn notice(&mut self, message: &str) -> io::Result<()> {
        write_label(
            &mut self.stdout,
            self.stdout_colors,
            AnsiColor::BrightBlack.on_default(),
            "•",
        )?;
        writeln!(self.stdout, " {message}")
    }

    fn empty(&mut self, message: &str) -> io::Result<()> {
        write_label(
            &mut self.stdout,
            self.stdout_colors,
            AnsiColor::BrightBlack.on_default(),
            message,
        )?;
        writeln!(self.stdout)
    }

    fn details<'a>(
        &mut self,
        rows: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> io::Result<()> {
        let mut table = self.detail_table();
        for (key, value) in rows {
            table.add_row([self.detail_key(key), Cell::new(value)]);
        }
        self.write_table(&table)
    }

    fn table<const N: usize>(&self, headers: [&str; N]) -> Table {
        let mut table = Table::new();
        table
            .load_preset(NOTHING)
            .set_content_arrangement(ContentArrangement::Dynamic)
            .set_header(headers.map(|header| {
                let cell = Cell::new(header);
                if self.stdout_colors {
                    cell.fg(Color::DarkGrey).add_attribute(Attribute::Bold)
                } else {
                    cell
                }
            }));
        table
    }

    fn detail_table(&self) -> Table {
        let mut table = Table::new();
        table
            .load_preset(NOTHING)
            .set_content_arrangement(ContentArrangement::Dynamic);
        table
    }

    fn detail_key(&self, value: &str) -> Cell {
        let cell = Cell::new(value);
        if self.stdout_colors {
            cell.fg(Color::DarkGrey).add_attribute(Attribute::Bold)
        } else {
            cell
        }
    }

    fn status_cell(&self, value: &str) -> Cell {
        let color = match value {
            "running" | "completed" | "current" => Color::Green,
            "starting" | "launching" | "recovering" | "restart-backoff" | "stopping" => {
                Color::Yellow
            }
            "failed" | "launch-failed" | "supervisor-lost" | "outcome-unknown"
            | "definition-missing" | "not registered" => Color::Red,
            _ => Color::DarkGrey,
        };
        color_cell(value, color, self.stdout_colors)
    }

    fn bool_cell(&self, value: bool) -> Cell {
        if value {
            color_cell("yes", Color::Green, self.stdout_colors)
        } else {
            color_cell("no", Color::DarkGrey, self.stdout_colors)
        }
    }

    fn notes_cell(&self, workload: &Workload) -> Cell {
        color_cell(
            workload_notes(workload).join(", "),
            Color::Yellow,
            self.stdout_colors,
        )
    }

    fn write_table(&mut self, table: &Table) -> io::Result<()> {
        writeln!(self.stdout, "{table}")
    }
}

fn write_log_message(
    writer: &mut impl Write,
    line_start: &mut bool,
    colors: bool,
    record: &ReadLogsResponse,
    options: LogOptions,
) -> io::Result<()> {
    if !options.timestamps && !options.prefix {
        writer.write_all(&record.message)?;
        if let Some(last) = record.message.last() {
            *line_start = *last == b'\n';
        }
        return writer.flush();
    }
    for part in record.message.split_inclusive(|byte| *byte == b'\n') {
        if *line_start {
            write_log_prefix(writer, colors, record, options)?;
        }
        writer.write_all(part)?;
        *line_start = part.ends_with(b"\n");
    }
    writer.flush()
}

fn write_log_prefix(
    writer: &mut impl Write,
    colors: bool,
    record: &ReadLogsResponse,
    options: LogOptions,
) -> io::Result<()> {
    if options.timestamps {
        write_label(
            writer,
            colors,
            AnsiColor::BrightBlack.on_default(),
            &format_timestamp(record.timestamp_unix_ms),
        )?;
        write!(writer, " ")?;
    }
    if options.prefix {
        let color = if record.stream == "stderr" {
            AnsiColor::Magenta
        } else {
            AnsiColor::Cyan
        };
        write_label(
            writer,
            colors,
            color.on_default().bold(),
            &format!("{}#{}", record.stream, record.attempt),
        )?;
        write_label(writer, colors, AnsiColor::BrightBlack.on_default(), " | ")?;
    }
    Ok(())
}

fn write_label(writer: &mut impl Write, colors: bool, style: Style, value: &str) -> io::Result<()> {
    if colors {
        write!(writer, "{}{value}{}", style.render(), style.render_reset())
    } else {
        write!(writer, "{value}")
    }
}

fn color_cell(value: impl Display, color: Color, colors: bool) -> Cell {
    let cell = Cell::new(value);
    if colors { cell.fg(color) } else { cell }
}

fn workload_notes(workload: &Workload) -> Vec<&'static str> {
    let mut notes = Vec::new();
    if workload.definition_missing {
        notes.push("definition missing");
    }
    if workload.restart_required {
        notes.push("restart required");
    }
    if workload.policy_sync_pending {
        notes.push("policy sync pending");
    }
    notes
}

fn optional_number(value: u32) -> String {
    if value == 0 {
        "-".to_owned()
    } else {
        value.to_string()
    }
}

fn shorten(value: &str, length: usize) -> &str {
    value.get(..length).unwrap_or(value)
}

fn format_timestamp(unix_ms: i64) -> String {
    if unix_ms == 0 {
        return "-".to_owned();
    }
    jiff::Timestamp::from_millisecond(unix_ms).map_or_else(
        |_| unix_ms.to_string(),
        |timestamp| timestamp.strftime("%Y-%m-%dT%H:%M:%S.%3fZ").to_string(),
    )
}
