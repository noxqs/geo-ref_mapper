//! Small vector glyphs painted directly, so the chrome never depends on which
//! symbol fonts happen to be installed.

use eframe::egui::{
    Color32, CornerRadius, Painter, Pos2, Rect, Shape, Stroke, StrokeKind, Vec2, pos2, vec2,
};

fn stroke(color: Color32, width: f32) -> Stroke {
    Stroke::new(width, color)
}

/// The app mark: a photo frame with a sun and a ridge line.
pub fn app_mark(painter: &Painter, rect: Rect, tint: Color32) {
    let frame = rect.shrink(rect.width() * 0.08);
    painter.rect_stroke(
        frame,
        CornerRadius::same(3),
        stroke(tint, 1.4),
        StrokeKind::Inside,
    );

    let sun = frame.left_top() + vec2(frame.width() * 0.28, frame.height() * 0.3);
    painter.circle_filled(sun, frame.width() * 0.1, tint);

    let base = frame.bottom() - frame.height() * 0.18;
    let ridge = vec![
        pos2(frame.left() + frame.width() * 0.12, base),
        pos2(
            frame.left() + frame.width() * 0.42,
            base - frame.height() * 0.34,
        ),
        pos2(
            frame.left() + frame.width() * 0.62,
            base - frame.height() * 0.12,
        ),
        pos2(
            frame.left() + frame.width() * 0.76,
            base - frame.height() * 0.3,
        ),
        pos2(frame.right() - frame.width() * 0.12, base),
    ];
    painter.add(Shape::convex_polygon(ridge, tint, Stroke::NONE));
}

pub fn minimize(painter: &Painter, rect: Rect, tint: Color32) {
    let y = rect.center().y;
    let half = rect.width() * 0.3;
    painter.line_segment(
        [
            pos2(rect.center().x - half, y),
            pos2(rect.center().x + half, y),
        ],
        stroke(tint, 1.3),
    );
}

pub fn maximize(painter: &Painter, rect: Rect, tint: Color32) {
    let square = Rect::from_center_size(rect.center(), Vec2::splat(rect.width() * 0.56));
    painter.rect_stroke(
        square,
        CornerRadius::same(2),
        stroke(tint, 1.3),
        StrokeKind::Inside,
    );
}

/// Two offset squares — the "restore down" glyph shown while maximized.
pub fn restore(painter: &Painter, rect: Rect, tint: Color32) {
    let size = rect.width() * 0.48;
    let offset = size * 0.28;
    let front = Rect::from_min_size(
        rect.center() - Vec2::splat(size * 0.5) + vec2(-offset * 0.5, offset * 0.5),
        Vec2::splat(size),
    );
    let back = front.translate(vec2(offset, -offset));

    painter.rect_stroke(
        back,
        CornerRadius::same(2),
        stroke(tint.gamma_multiply(0.7), 1.2),
        StrokeKind::Inside,
    );
    painter.rect_filled(front, CornerRadius::same(2), Color32::TRANSPARENT);
    painter.rect_stroke(
        front,
        CornerRadius::same(2),
        stroke(tint, 1.3),
        StrokeKind::Inside,
    );
}

pub fn close(painter: &Painter, rect: Rect, tint: Color32) {
    let r = rect.width() * 0.28;
    let c = rect.center();
    let s = stroke(tint, 1.4);
    painter.line_segment([c + vec2(-r, -r), c + vec2(r, r)], s);
    painter.line_segment([c + vec2(r, -r), c + vec2(-r, r)], s);
}

/// A window outline with one side filled in — used for the panel toggles.
/// Which edge of the window a toggle's panel is attached to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PanelSide {
    Left,
    Right,
    /// The filmstrip along the bottom of the Viewer.
    Bottom,
}

pub fn panel_toggle(painter: &Painter, rect: Rect, tint: Color32, side: PanelSide, active: bool) {
    let frame =
        Rect::from_center_size(rect.center(), vec2(rect.width() * 0.62, rect.width() * 0.5));
    painter.rect_stroke(
        frame,
        CornerRadius::same(2),
        stroke(tint, 1.3),
        StrokeKind::Inside,
    );

    let bar = match side {
        PanelSide::Left => Rect::from_min_max(
            frame.min,
            pos2(frame.left() + frame.width() * 0.36, frame.bottom()),
        ),
        PanelSide::Right => Rect::from_min_max(
            pos2(frame.right() - frame.width() * 0.36, frame.top()),
            frame.max,
        ),
        // A shallower band, matching how the filmstrip actually sits.
        PanelSide::Bottom => Rect::from_min_max(
            pos2(frame.left(), frame.bottom() - frame.height() * 0.38),
            frame.max,
        ),
    };
    let fill = if active {
        tint
    } else {
        tint.gamma_multiply(0.35)
    };
    painter.rect_filled(bar.shrink(1.0), CornerRadius::same(1), fill);
}

/// Disclosure chevron. `openness` of 0 points right, 1 points down.
pub fn chevron(painter: &Painter, rect: Rect, tint: Color32, openness: f32) {
    let r = rect.width() * 0.2;
    let c = rect.center();
    let angle = openness * std::f32::consts::FRAC_PI_2;
    let (sin, cos) = angle.sin_cos();
    let rotate = |v: Vec2| vec2(v.x * cos - v.y * sin, v.x * sin + v.y * cos);

    let tip = c + rotate(vec2(r, 0.0));
    let top = c + rotate(vec2(-r * 0.6, -r));
    let bottom = c + rotate(vec2(-r * 0.6, r));

    let s = stroke(tint, 1.4);
    painter.line_segment([top, tip], s);
    painter.line_segment([tip, bottom], s);
}

pub fn folder(painter: &Painter, rect: Rect, tint: Color32, open: bool) {
    let w = rect.width() * 0.78;
    let h = w * 0.76;
    let body = Rect::from_center_size(rect.center(), vec2(w, h));

    // Tab along the top-left edge.
    let tab = Rect::from_min_size(body.min, vec2(w * 0.45, h * 0.22));
    painter.rect_filled(tab, CornerRadius::same(1), tint.gamma_multiply(0.75));

    let front = Rect::from_min_max(pos2(body.left(), body.top() + h * 0.16), body.max);
    let fill = if open {
        tint.gamma_multiply(0.85)
    } else {
        tint.gamma_multiply(0.55)
    };
    painter.rect_filled(front, CornerRadius::same(2), fill);
}

pub fn image_file(painter: &Painter, rect: Rect, tint: Color32) {
    let w = rect.width() * 0.72;
    let body = Rect::from_center_size(rect.center(), vec2(w, w * 0.82));
    painter.rect_stroke(
        body,
        CornerRadius::same(2),
        stroke(tint, 1.2),
        StrokeKind::Inside,
    );
    painter.circle_filled(
        body.left_top() + vec2(body.width() * 0.3, body.height() * 0.3),
        body.width() * 0.09,
        tint,
    );
    let base = body.bottom() - body.height() * 0.16;
    painter.add(Shape::convex_polygon(
        vec![
            pos2(body.left() + body.width() * 0.14, base),
            pos2(
                body.left() + body.width() * 0.46,
                base - body.height() * 0.36,
            ),
            pos2(body.right() - body.width() * 0.14, base),
        ],
        tint.gamma_multiply(0.8),
        Stroke::NONE,
    ));
}

/// Checkerboard used behind the (still empty) image canvas.
/// A graphics chip: a die with contact legs down each side.
///
/// Drawn rather than shipped as artwork, like everything else here. The vendors'
/// real logos are registered trademarks, so the badge identifies a card by its
/// maker's brand colour and this neutral mark instead of reproducing one.
pub fn gpu_chip(painter: &Painter, rect: Rect, tint: Color32) {
    let size = rect.width().min(rect.height()) * 0.62;
    let body = Rect::from_center_size(rect.center(), vec2(size, size));

    painter.rect_stroke(
        body,
        CornerRadius::same(2),
        stroke(tint, 1.3),
        StrokeKind::Inside,
    );
    // The die itself, a smaller square inside.
    painter.rect_filled(
        Rect::from_center_size(rect.center(), vec2(size * 0.42, size * 0.42)),
        CornerRadius::same(1),
        tint.gamma_multiply(0.85),
    );

    // Three legs on each side, which is what reads as "chip" at this size.
    let leg = size * 0.22;
    for index in 0..3 {
        let y = body.top() + size * (0.28 + index as f32 * 0.22);
        painter.line_segment(
            [pos2(body.left() - leg * 0.5, y), pos2(body.left(), y)],
            stroke(tint, 1.1),
        );
        painter.line_segment(
            [pos2(body.right(), y), pos2(body.right() + leg * 0.5, y)],
            stroke(tint, 1.1),
        );
    }
}

/// A processor, for when nothing is drawing on hardware at all: the same die
/// but with a cross through it, so software rendering never reads as a GPU.
pub fn cpu_chip(painter: &Painter, rect: Rect, tint: Color32) {
    gpu_chip(painter, rect, tint);
    let size = rect.width().min(rect.height()) * 0.62;
    let inner = Rect::from_center_size(rect.center(), vec2(size * 0.42, size * 0.42));
    painter.line_segment(
        [inner.left_top(), inner.right_bottom()],
        stroke(tint.gamma_multiply(0.5), 1.0),
    );
}

pub fn checkerboard(painter: &Painter, rect: Rect, cell: f32, a: Color32, b: Color32) {
    painter.rect_filled(rect, CornerRadius::ZERO, a);
    let cols = (rect.width() / cell).ceil() as i32;
    let rows = (rect.height() / cell).ceil() as i32;
    for row in 0..rows {
        for col in 0..cols {
            if (row + col) % 2 == 0 {
                continue;
            }
            let min = Pos2::new(
                rect.left() + col as f32 * cell,
                rect.top() + row as f32 * cell,
            );
            let tile = Rect::from_min_size(min, Vec2::splat(cell)).intersect(rect);
            painter.rect_filled(tile, CornerRadius::ZERO, b);
        }
    }
}
