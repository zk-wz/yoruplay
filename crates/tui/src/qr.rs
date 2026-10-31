//! QR matrix generation shared by the TUI and the CLI login command.

pub fn matrix(data: &str) -> Result<Vec<Vec<bool>>, qrcode::types::QrError> {
    let code = qrcode::QrCode::new(data.as_bytes())?;
    let width = code.width();
    let colors = code.to_colors();
    let mut rows = Vec::with_capacity(width);
    for y in 0..width {
        let mut row = Vec::with_capacity(width);
        for x in 0..width {
            row.push(colors[y * width + x] == qrcode::Color::Dark);
        }
        rows.push(row);
    }
    Ok(rows)
}

/// Render with ANSI black-on-white half blocks so the code remains scannable in
/// dark and light terminals. `true` means a dark module.
pub fn render_ansi(data: &str) -> Result<Vec<String>, qrcode::types::QrError> {
    let matrix = matrix(data)?;
    let quiet = 4usize;
    let size = matrix.len() + quiet * 2;
    let dark = '\u{2588}'; // full block
    let upper = '\u{2580}';
    let lower = '\u{2584}';
    let mut lines = Vec::new();
    let mut y = 0;
    while y < size {
        let mut line = String::from("\x1b[30m\x1b[107m");
        for x in 0..size {
            let top = module(&matrix, quiet, x, y);
            let bottom = module(&matrix, quiet, x, y + 1);
            line.push(match (top, bottom) {
                (true, true) => dark,
                (true, false) => upper,
                (false, true) => lower,
                (false, false) => ' ',
            });
        }
        line.push_str("\x1b[0m");
        lines.push(line);
        y += 2;
    }
    Ok(lines)
}

/// Self-contained SVG for headless login (white background so phone cameras
/// can scan it in any viewer).
pub fn render_svg(data: &str) -> Result<String, qrcode::types::QrError> {
    let matrix = matrix(data)?;
    let quiet = 4usize;
    let size = matrix.len() + quiet * 2;
    let display = size * 8;
    let mut path = String::new();
    for (y, row) in matrix.iter().enumerate() {
        for (x, dark) in row.iter().enumerate() {
            if *dark {
                path.push_str(&format!("M{} {}h1v1h-1z", x + quiet, y + quiet));
            }
        }
    }
    Ok(format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {size} {size}\" width=\"{display}\" height=\"{display}\" shape-rendering=\"crispEdges\"><rect width=\"100%\" height=\"100%\" fill=\"#fff\"/><path fill=\"#000\" d=\"{path}\"/></svg>\n"
    ))
}

fn module(matrix: &[Vec<bool>], quiet: usize, x: usize, y: usize) -> bool {
    let ix = x as isize - quiet as isize;
    let iy = y as isize - quiet as isize;
    if ix < 0 || iy < 0 {
        return false;
    }
    matrix
        .get(iy as usize)
        .and_then(|row| row.get(ix as usize))
        .copied()
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_has_expected_size_and_quiet_zone() {
        let m = matrix("https://music.163.com/login?codekey=abc").unwrap();
        assert!(m.len() > 20);
        assert!(m.iter().all(|row| row.len() == m.len()));
        let ansi = render_ansi("https://music.163.com/login?codekey=abc").unwrap();
        assert!(!ansi.is_empty());
        assert!(ansi[0].starts_with("\x1b[30m\x1b[107m"));
    }

    #[test]
    fn svg_is_self_contained() {
        let svg = render_svg("https://music.163.com/login?codekey=abc").unwrap();
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains("fill=\"#fff\""));
        assert!(svg.contains("fill=\"#000\""));
    }
}
