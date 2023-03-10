use rustc_middle::thir::*;

#[derive(Debug, PartialEq)]
pub(crate) enum Category {
    /// An assignable memory location like `x`, `x.f`, `foo()[3]`, that
    /// sort of thing. Something that could appear on the LHS of an `=`
    /// sign.
    Place,

    /// A literal like `23` or `"foo"`. Does not include constant
    /// expressions like `3 + 5`.
    Constant,

    /// Something that generates a new value at runtime, like `x + y`
    /// or `foo()`.
    Rvalue(RvalueFunc),
}

/// Rvalues fall into different "styles" that will determine which fn
/// is best suited to generate them.
#[derive(Debug, PartialEq)]
pub(crate) enum RvalueFunc {
    /// Best generated by `into`. This is generally exprs that
    /// cause branching, like `match`, but also includes calls.
    Into,

    /// Best generated by `as_rvalue`. This is usually the case.
    AsRvalue,
}

impl Category {
    /// Determines the category for a given expression. Note that scope
    /// and paren expressions have no category.
    pub(crate) fn of(ek: &ExprKind<'_>) -> Option<Category> {
        match *ek {
            ExprKind::Scope { .. } => None,

            ExprKind::Field { .. }
            | ExprKind::Deref { .. }
            | ExprKind::Index { .. }
            | ExprKind::UpvarRef { .. }
            | ExprKind::VarRef { .. }
            | ExprKind::PlaceTypeAscription { .. }
            | ExprKind::ValueTypeAscription { .. } => Some(Category::Place),

            ExprKind::LogicalOp { .. }
            | ExprKind::Match { .. }
            | ExprKind::If { .. }
            | ExprKind::Let { .. }
            | ExprKind::NeverToAny { .. }
            | ExprKind::Use { .. }
            | ExprKind::Adt { .. }
            | ExprKind::Borrow { .. }
            | ExprKind::AddressOf { .. }
            | ExprKind::Yield { .. }
            | ExprKind::Call { .. }
            | ExprKind::InlineAsm { .. } => Some(Category::Rvalue(RvalueFunc::Into)),

            ExprKind::Array { .. }
            | ExprKind::Tuple { .. }
            | ExprKind::Closure { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Binary { .. }
            | ExprKind::Box { .. }
            | ExprKind::Cast { .. }
            | ExprKind::Pointer { .. }
            | ExprKind::Repeat { .. }
            | ExprKind::Assign { .. }
            | ExprKind::AssignOp { .. }
            | ExprKind::ThreadLocalRef(_) => Some(Category::Rvalue(RvalueFunc::AsRvalue)),

            ExprKind::ConstBlock { .. }
            | ExprKind::Literal { .. }
            | ExprKind::NonHirLiteral { .. }
            | ExprKind::ZstLiteral { .. }
            | ExprKind::ConstParam { .. }
            | ExprKind::StaticRef { .. }
            | ExprKind::NamedConst { .. } => Some(Category::Constant),

            ExprKind::Loop { .. }
            | ExprKind::Block { .. }
            | ExprKind::Break { .. }
            | ExprKind::Continue { .. }
            | ExprKind::Return { .. } =>
            // FIXME(#27840) these probably want their own
            // category, like "nonterminating"
            {
                Some(Category::Rvalue(RvalueFunc::Into))
            }
        }
    }
}