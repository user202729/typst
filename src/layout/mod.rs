//! The core layouting engine.

use std::io::{self, Write};

use smallvec::SmallVec;

use toddle::query::{FontClass, SharedFontLoader};
use toddle::Error as FontError;

use crate::func::Command;
use crate::size::{Size, Size2D, SizeBox};
use crate::style::{LayoutStyle, TextStyle};
use crate::syntax::{FuncCall, Node, SyntaxTree};

mod actions;
mod tree;
mod flex;
mod stack;
mod text;

/// Different kinds of layouters (fully re-exported).
pub mod layouters {
    pub use super::tree::layout_tree;
    pub use super::flex::{FlexLayouter, FlexContext};
    pub use super::stack::{StackLayouter, StackContext};
    pub use super::text::{layout_text, TextContext};
}

pub use actions::{LayoutAction, LayoutActionList};
pub use layouters::*;

/// A collection of layouts.
pub type MultiLayout = Vec<Layout>;

/// A sequence of layouting actions inside a box.
#[derive(Debug, Clone)]
pub struct Layout {
    /// The size of the box.
    pub dimensions: Size2D,
    /// The baseline of the layout (as an offset from the top-left).
    pub baseline: Option<Size>,
    /// How to align this layout in a parent container.
    pub alignment: LayoutAlignment,
    /// The actions composing this layout.
    pub actions: Vec<LayoutAction>,
}

/// The general context for layouting.
#[derive(Debug, Clone)]
pub struct LayoutContext<'a, 'p> {
    /// The font loader to retrieve fonts from when typesetting text
    /// using [`layout_text`].
    pub loader: &'a SharedFontLoader<'p>,
    /// The style for pages and text.
    pub style: &'a LayoutStyle,
    /// Whether this layouting process handles the top-level pages.
    pub top_level: bool,
    /// The spaces to layout in.
    pub spaces: LayoutSpaces,
    /// The initial axes along which content is laid out.
    pub axes: LayoutAxes,
    /// The alignment for the two axes.
    pub alignment: LayoutAlignment,
}

/// A possibly stack-allocated vector of layout spaces.
pub type LayoutSpaces = SmallVec<[LayoutSpace; 2]>;

/// Spacial layouting constraints.
#[derive(Debug, Copy, Clone)]
pub struct LayoutSpace {
    /// The maximum size of the box to layout in.
    pub dimensions: Size2D,
    /// Whether to expand the dimensions of the resulting layout to the full
    /// dimensions of this space or to shrink them to fit the content for the
    /// vertical and horizontal axis.
    pub expand: (bool, bool),
    /// Padding that should be respected on each side.
    pub padding: SizeBox,
}

impl LayoutSpace {
    /// The offset from the origin to the start of content, that is,
    /// `(padding.left, padding.top)`.
    pub fn start(&self) -> Size2D {
        Size2D::new(self.padding.left, self.padding.right)
    }

    /// The actually usable area (dimensions minus padding).
    pub fn usable(&self) -> Size2D {
        self.dimensions.unpadded(self.padding)
    }

    /// A layout space without padding and dimensions reduced by the padding.
    pub fn usable_space(&self) -> LayoutSpace {
        LayoutSpace {
            dimensions: self.usable(),
            expand: (false, false),
            padding: SizeBox::zero(),
        }
    }
}

/// The axes along which the content is laid out.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct LayoutAxes {
    pub primary: Axis,
    pub secondary: Axis,
}

impl LayoutAxes {
    pub fn new(primary: Axis, secondary: Axis) -> LayoutAxes {
        LayoutAxes { primary, secondary }
    }

    /// Returns the generalized version of a `Size2D` dependent on
    /// the layouting axes, that is:
    /// - The x coordinate describes the primary axis instead of the horizontal one.
    /// - The y coordinate describes the secondary axis instead of the vertical one.
    pub fn generalize(&self, size: Size2D) -> Size2D {
        if self.primary.is_horizontal() {
            size
        } else {
            Size2D { x: size.y, y: size.x }
        }
    }

    /// Returns the specialized version of this generalized Size2D.
    /// (Inverse to `generalized`).
    pub fn specialize(&self, size: Size2D) -> Size2D {
        // In fact, generalized is its own inverse. For reasons of clarity
        // at the call site, we still have this second function.
        self.generalize(size)
    }
}

/// Directions along which content is laid out.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum Axis {
    LeftToRight,
    RightToLeft,
    TopToBottom,
    BottomToTop,
}

impl Axis {
    /// Whether this is a horizontal axis.
    pub fn is_horizontal(&self) -> bool {
        match self {
            Axis::LeftToRight | Axis::RightToLeft => true,
            Axis::TopToBottom | Axis::BottomToTop => false,
        }
    }

    /// Whether this axis points into the positive coordinate direction.
    pub fn is_positive(&self) -> bool {
        match self {
            Axis::LeftToRight | Axis::TopToBottom => true,
            Axis::RightToLeft | Axis::BottomToTop => false,
        }
    }

    /// The direction factor for this axis.
    ///
    /// - 1 if the axis is positive.
    /// - -1 if the axis is negative.
    pub fn factor(&self) -> i32 {
        if self.is_positive() { 1 } else { -1 }
    }
}

/// The place to put a layout in a container.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct LayoutAlignment {
    pub primary: Alignment,
    pub secondary: Alignment,
}

impl LayoutAlignment {
    pub fn new(primary: Alignment, secondary: Alignment) -> LayoutAlignment {
        LayoutAlignment { primary, secondary }
    }
}

/// Where to align content.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum Alignment {
    Origin,
    Center,
    End,
}

/// The specialized anchor position for an item with the given alignment in a
/// container with a given size along the given axis.
pub fn anchor(axis: Axis, size: Size, alignment: Alignment) -> Size {
    use Alignment::*;
    match (axis.is_positive(), alignment) {
        (true, Origin) | (false, End) => Size::zero(),
        (_, Center) => size / 2,
        (true, End) | (false, Origin) => size,
    }
}

/// Whitespace between boxes with different interaction properties.
#[derive(Debug, Copy, Clone, PartialEq)]
pub enum SpacingKind {
    /// A hard space consumes surrounding soft spaces and is always layouted.
    Hard,
    /// A soft space consumes surrounding soft spaces with higher value.
    Soft(u32),
}

/// The standard spacing kind used for paragraph spacing.
const PARAGRAPH_KIND: SpacingKind = SpacingKind::Soft(1);

/// The standard spacing kind used for normal spaces between boxes.
const SPACE_KIND: SpacingKind = SpacingKind::Soft(2);

/// The last appeared spacing.
#[derive(Debug, Copy, Clone, PartialEq)]
enum LastSpacing {
    Hard,
    Soft(Size, u32),
    None,
}

impl LastSpacing {
    fn soft_or_zero(&self) -> Size {
        match self {
            LastSpacing::Soft(space, _) => *space,
            _ => Size::zero(),
        }
    }
}

/// Layout components that can be serialized.
trait Serialize {
    /// Serialize the data structure into an output writable.
    fn serialize<W: Write>(&self, f: &mut W) -> io::Result<()>;
}

impl Serialize for Layout {
    fn serialize<W: Write>(&self, f: &mut W) -> io::Result<()> {
        writeln!(f, "{:.4} {:.4}", self.dimensions.x.to_pt(), self.dimensions.y.to_pt())?;
        writeln!(f, "{}", self.actions.len())?;
        for action in &self.actions {
            action.serialize(f)?;
            writeln!(f)?;
        }
        Ok(())
    }
}

impl Serialize for MultiLayout {
    fn serialize<W: Write>(&self, f: &mut W) -> io::Result<()> {
        writeln!(f, "{}", self.len())?;
        for layout in self {
            layout.serialize(f)?;
        }
        Ok(())
    }
}

/// The error type for layouting.
pub struct LayoutError(String);

/// The result type for layouting.
pub type LayoutResult<T> = Result<T, LayoutError>;

impl LayoutError {
    /// Create a new layout error with a message.
    pub fn new<S: Into<String>>(message: S) -> LayoutError {
        LayoutError(message.into())
    }
}

error_type! {
    err: LayoutError,
    show: f => f.write_str(&err.0),
    from: (std::io::Error, LayoutError::new(err.to_string())),
    from: (FontError, LayoutError::new(err.to_string())),
}
