use crate::frame::RenderFrame;

pub fn ansi_to_html(input: &str) -> String {
    let mut out = String::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            while let Some(c) = chars.next() {
                if c == 'm' {
                    break;
                }
            }
            continue;
        }
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

pub fn render_frame_to_svg(frame: &RenderFrame) -> String {
    let cell_w = 8usize;
    let cell_h = 12usize;
    let baseline = 10usize;
    let width_chars = frame
        .chars
        .iter()
        .map(|row| row.chars().count())
        .max()
        .unwrap_or(0);
    let height_chars = frame.chars.len();
    let width = width_chars * cell_w;
    let height = height_chars * cell_h;

    let mut svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">
<rect width="100%" height="100%" fill="#101014"/>
<style>text{{font-family:Menlo,Monaco,'Courier New',monospace;font-size:12px;white-space:pre}}</style>
"##
    );

    for (y, row) in frame.chars.iter().enumerate() {
        if let Some(colors) = &frame.colors {
            let mut current_color: Option<(u8, u8, u8)> = None;
            let mut run = String::new();
            let mut run_start = 0usize;

            for (x, ch) in row.chars().enumerate() {
                let color = colors
                    .get(y)
                    .and_then(|row| row.get(x))
                    .copied()
                    .unwrap_or((220, 220, 220));
                if current_color == Some(color) {
                    run.push(ch);
                } else {
                    push_svg_text_run(
                        &mut svg,
                        run_start,
                        y,
                        baseline,
                        cell_w,
                        cell_h,
                        current_color,
                        &run,
                    );
                    current_color = Some(color);
                    run.clear();
                    run.push(ch);
                    run_start = x;
                }
            }
            push_svg_text_run(
                &mut svg,
                run_start,
                y,
                baseline,
                cell_w,
                cell_h,
                current_color,
                &run,
            );
        } else {
            push_svg_text_run(
                &mut svg,
                0,
                y,
                baseline,
                cell_w,
                cell_h,
                Some((220, 220, 220)),
                row,
            );
        }
    }

    svg.push_str("</svg>\n");
    svg
}

fn push_svg_text_run(
    svg: &mut String,
    x: usize,
    y: usize,
    baseline: usize,
    cell_w: usize,
    cell_h: usize,
    color: Option<(u8, u8, u8)>,
    text: &str,
) {
    if text.is_empty() {
        return;
    }
    let (r, g, b) = color.unwrap_or((220, 220, 220));
    svg.push_str(&format!(
        r#"<text x="{}" y="{}" fill="rgb({r},{g},{b})">{}</text>
"#,
        x * cell_w,
        y * cell_h + baseline,
        escape_xml(text)
    ));
}

fn escape_xml(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}
