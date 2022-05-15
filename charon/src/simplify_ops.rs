//! In MIR, many binops are desugared into:
//! * for division/modulo: a test that the divider is non zero (making the code
//!   panics if the divider is zero), then the division itself
//! * an operation, followed by a test: typically an addition followed by a check
//!   for overflow
//! This is a bit too low-level for us: we only want to have the binop (which will
//! have a precondition in our theorem prover, or will be monadic...). We thus want
//! to remove those unnecessary checks.

use crate::expressions::*;
use crate::llbc_ast::{Assert, FunDecl, FunDecls, Statement, SwitchTargets};
use crate::types::*;
use crate::values::*;
use std::iter::FromIterator;

/// Return true iff: `place ++ [pelem] == full_place`
fn check_places_similar_but_last_proj_elem(
    place: &Place,
    pelem: &ProjectionElem,
    full_place: &Place,
) -> bool {
    if place.var_id == full_place.var_id
        && place.projection.len() + 1 == full_place.projection.len()
    {
        for i in 0..place.projection.len() {
            if place.projection[i] != full_place.projection[i] {
                return false;
            }
        }

        return *pelem == full_place.projection[place.projection.len()];
    }
    return false;
}

/// Return true if the binary operation might fail and thus requires its result
/// to be checked (overflows, for instance).
fn binop_requires_assert_after(binop: BinOp) -> bool {
    match binop {
        BinOp::BitXor
        | BinOp::BitAnd
        | BinOp::BitOr
        | BinOp::Eq
        | BinOp::Lt
        | BinOp::Le
        | BinOp::Ne
        | BinOp::Ge
        | BinOp::Gt
        | BinOp::Div
        | BinOp::Rem => false,
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Shl | BinOp::Shr => true,
    }
}

/// Return true if the binary operation has a precondition (divisor is non zero
/// for instance) and must thus be preceded by an assertion.
fn binop_requires_assert_before(binop: BinOp) -> bool {
    match binop {
        BinOp::BitXor
        | BinOp::BitAnd
        | BinOp::BitOr
        | BinOp::Eq
        | BinOp::Lt
        | BinOp::Le
        | BinOp::Ne
        | BinOp::Ge
        | BinOp::Gt
        | BinOp::Add
        | BinOp::Sub
        | BinOp::Mul
        | BinOp::Shl
        | BinOp::Shr => false,
        BinOp::Div | BinOp::Rem => true,
    }
}

fn binop_can_fail(binop: BinOp) -> bool {
    binop_requires_assert_after(binop) || binop_requires_assert_before(binop)
}

/// Check if this is a group of statements of the form:
/// - do an operation,
/// - check it succeeded (didn't overflow, etc.)
/// - retrieve the value
///   ```
///   ```
/// Check if this is a group of statements which should be collapsed to a
/// single checked binop.
/// Simply check if the first statements is a checked binop.
fn check_if_binop_then_assert(st1: &Statement, st2: &Statement, st3: &Statement) -> bool {
    match st1 {
        Statement::Assign(_, Rvalue::BinaryOp(binop, _, _)) => {
            if binop_requires_assert_after(*binop) {
                // We found a checked binary op.
                //
                // This group of statements should exactly match the following pattern:
                //   ```
                //   tmp := copy x + copy y; // Possibly a different binop
                //   assert(move (tmp.1) == false);
                //   dest := move (tmp.0);
                //   ...
                //   ```
                // If it is note the case, we can't collapse...
                check_if_simplifiable_binop_then_assert(st1, st2, st3);
                true
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Make sure the statements match the following pattern:
///   ```
///   tmp := op1 + op2; // Possibly a different binop
///   assert(move (tmp.1) == false);
///   dest := move (tmp.0);
///   ...
///   ```
fn check_if_simplifiable_binop_then_assert(st1: &Statement, st2: &Statement, st3: &Statement) {
    match (st1, st2, st3) {
        (
            Statement::Assign(bp, Rvalue::BinaryOp(binop, _op1, _op2)),
            Statement::Assert(Assert {
                cond: Operand::Move(cond_op),
                expected,
            }),
            Statement::Assign(_mp, Rvalue::Use(Operand::Move(mr))),
        ) => {
            assert!(binop_requires_assert_after(*binop));
            assert!(!(*expected));

            // We must have:
            // cond_op == bp.1
            // mr == bp.0
            let check1 = check_places_similar_but_last_proj_elem(
                bp,
                &ProjectionElem::Field(FieldProjKind::Tuple(2), FieldId::Id::new(1)),
                cond_op,
            );
            assert!(check1);

            let check2 = check_places_similar_but_last_proj_elem(
                bp,
                &ProjectionElem::Field(FieldProjKind::Tuple(2), FieldId::Id::new(0)),
                mr,
            );
            assert!(check2);
        }
        _ => {
            unreachable!();
        }
    }
}

/// Simplify patterns of the form:
///   ```
///   tmp := op1 + op2; // Possibly a different binop
///   assert(move (tmp.1) == false);
///   dest := move (tmp.0);
///   ...
///   ```
/// to:
///   ```
///   tmp := copy x + copy y; // Possibly a different binop
///   ...
///   ```
/// Note that the type of the binop changes in the two situations (in the
/// translation, before the transformation `+` returns a pair (bool, int),
/// after it has a monadic type).
fn simplify_binop_then_assert(st1: Statement, st2: Statement, st3: Statement) -> Statement {
    match (st1, st2, st3) {
        (Statement::Assign(_, binop), Statement::Assert(_), Statement::Assign(mp, _)) => {
            return Statement::Assign(mp, binop);
        }
        _ => {
            unreachable!();
        }
    }
}

/// Check if this is a group of statements of the form: "check that we can do
/// an binary operation, then do this operation (ex.: check that a divisor is
/// non zero before doing a division, panic otherwise)"
fn check_if_assert_then_binop(st1: &Statement, st2: &Statement, st3: &Statement) -> bool {
    match st3 {
        Statement::Assign(_, Rvalue::BinaryOp(binop, _, _)) => {
            if binop_requires_assert_before(*binop) {
                // We found an unchecked binop which should be simplified (division
                // or remainder computation).
                //
                // There are two situations:
                // - if the divisor is a non-zero constant, rust may not insert
                //   an assertion (because it can statically check it)
                // - otherwise, the group of statements must match the following
                //   pattern exactly:
                //   ```
                //   tmp := (copy divisor) == 0;
                //   assert((move tmp) == false);
                //   dest := move dividend / move divisor; // Can also be a `%`
                //   ...
                //   ```
                //
                //   Or this pattern:
                //   ```
                //   tmp := (constant_divisor) == 0;
                //   assert((move tmp) == false);
                //   dest := move dividend / constant_divisor; // Can also be a `%`
                //   ...
                //   ```
                check_if_simplifiable_assert_then_binop(st1, st2, st3)
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Make sure the statements match the following pattern:
///   ```
///   tmp := (copy divisor) == 0;
///   assert((move tmp) == false);
///   dest := move dividend / move divisor; // Can also be a `%`
///   ...
///   ```
/// Or that there is no assert but the divisor is a non-zero constant.
fn check_if_simplifiable_assert_then_binop(
    st1: &Statement,
    st2: &Statement,
    st3: &Statement,
) -> bool {
    match (st1, st2, st3) {
        (
            Statement::Assign(
                eq_dest,
                Rvalue::BinaryOp(
                    BinOp::Eq,
                    Operand::Copy(eq_op1),
                    Operand::Constant(
                        _,
                        OperandConstantValue::ConstantValue(ConstantValue::Scalar(zero)),
                    ),
                ),
            ),
            Statement::Assert(Assert {
                cond: Operand::Move(cond_op),
                expected,
            }),
            Statement::Assign(_mp, Rvalue::BinaryOp(binop, _dividend, Operand::Move(divisor))),
        ) => {
            // Case 1: pattern with copy/move and assertion
            assert!(binop_requires_assert_before(*binop));
            assert!(!(*expected));
            assert!(eq_op1 == divisor);
            assert!(eq_dest == cond_op);
            if zero.is_int() {
                assert!(zero.as_int().unwrap() == 0);
            } else {
                assert!(zero.as_uint().unwrap() == 0);
            }
            true
        }
        (
            Statement::Assign(
                eq_dest,
                Rvalue::BinaryOp(
                    BinOp::Eq,
                    divisor,
                    Operand::Constant(
                        _,
                        OperandConstantValue::ConstantValue(ConstantValue::Scalar(zero)),
                    ),
                ),
            ),
            Statement::Assert(Assert {
                cond: Operand::Move(cond_op),
                expected,
            }),
            Statement::Assign(_mp, Rvalue::BinaryOp(binop, _dividend, divisor1)),
        ) => {
            // Case 2: pattern with constant divisor and assertion
            assert!(binop_requires_assert_before(*binop));
            assert!(!(*expected));
            assert!(divisor.is_constant());
            match divisor {
                Operand::Constant(
                    _,
                    OperandConstantValue::ConstantValue(ConstantValue::Scalar(_)),
                ) => (),
                _ => unreachable!(),
            }
            assert!(divisor1 == divisor);
            assert!(eq_dest == cond_op);
            // Check that the zero is zero
            if zero.is_int() {
                assert!(zero.as_int().unwrap() == 0);
            } else {
                assert!(zero.as_uint().unwrap() == 0);
            }
            true
        }
        (_, _, Statement::Assign(_mp, Rvalue::BinaryOp(_, _, Operand::Constant(_, divisor)))) => {
            // Case 3: no assertion to check the divisor != 0, the divisor must be a
            // non-zero constant
            let cv = divisor.as_constant_value();
            let cv = cv.as_scalar();
            if cv.is_uint() {
                assert!(cv.as_uint().unwrap() != 0)
            } else {
                assert!(cv.as_int().unwrap() != 0)
            };
            false
        }
        _ => {
            unreachable!();
        }
    }
}

/// Simplify patterns of the form:
///   ```
///   tmp := (copy divisor) == 0;
///   assert((move tmp) == false);
///   dest := move dividend / move divisor; // Can also be a `%`
///   ...
///   ```
/// to:
///   ```
///   dest := move dividend / move divisor; // Can also be a `%`
///   ...
///   ```
fn simplify_assert_then_binop(_st1: Statement, _st2: Statement, st3: Statement) -> Statement {
    st3
}

/// Attempt to simplify a sequence of statemnets
fn simplify_st_seq(
    st1: Statement,
    st2: Statement,
    st3: Statement,
    st4: Option<Statement>,
) -> Statement {
    // Simplify checked binops
    if check_if_binop_then_assert(&st1, &st2, &st3) {
        let st = simplify_binop_then_assert(st1, st2, st3);
        match st4 {
            Option::Some(st4) => {
                let st4 = simplify_st(st4);
                return Statement::Sequence(Box::new(st), Box::new(st4));
            }
            Option::None => return st,
        }
    }
    // Simplify unchecked binops (division, modulo)
    if check_if_assert_then_binop(&st1, &st2, &st3) {
        let st = simplify_assert_then_binop(st1, st2, st3);
        match st4 {
            Option::Some(st4) => {
                let st4 = simplify_st(st4);
                return Statement::Sequence(Box::new(st), Box::new(st4));
            }
            Option::None => return st,
        }
    }
    // Not simplifyable
    let next_st = match st4 {
        Option::Some(st4) => Statement::Sequence(Box::new(st3), Box::new(st4)),
        Option::None => st3,
    };
    let next_st = Statement::Sequence(Box::new(st2), Box::new(next_st));
    Statement::Sequence(Box::new(simplify_st(st1)), Box::new(simplify_st(next_st)))
}

fn simplify_st(st: Statement) -> Statement {
    match st {
        Statement::Assign(p, rv) => {
            // Check that we never failed to simplify a binop
            match &rv {
                Rvalue::BinaryOp(binop, _, divisor) => {
                    // If it is an unsimplified binop, it must be / or %
                    // and the divisor must be a non-zero constant
                    if binop_can_fail(*binop) {
                        match binop {
                            BinOp::Div | BinOp::Rem => {
                                let (_, cv) = divisor.as_constant();
                                let cv = cv.as_constant_value();
                                let cv = cv.as_scalar();
                                if cv.is_uint() {
                                    assert!(cv.as_uint().unwrap() != 0)
                                } else {
                                    assert!(cv.as_int().unwrap() != 0)
                                };
                            }
                            _ => {
                                unreachable!();
                            }
                        }
                    }
                }
                _ => (),
            }
            Statement::Assign(p, rv)
        }
        Statement::FakeRead(p) => Statement::FakeRead(p),
        Statement::SetDiscriminant(p, vid) => Statement::SetDiscriminant(p, vid),
        Statement::Drop(p) => Statement::Drop(p),
        Statement::Assert(assert) => Statement::Assert(assert),
        Statement::Call(call) => Statement::Call(call),
        Statement::Panic => Statement::Panic,
        Statement::Return => Statement::Return,
        Statement::Break(i) => Statement::Break(i),
        Statement::Continue(i) => Statement::Continue(i),
        Statement::Nop => Statement::Nop,
        Statement::Switch(op, targets) => {
            let targets = match targets {
                SwitchTargets::If(st1, st2) => {
                    SwitchTargets::If(Box::new(simplify_st(*st1)), Box::new(simplify_st(*st2)))
                }
                SwitchTargets::SwitchInt(int_ty, targets, otherwise) => {
                    let targets =
                        Vec::from_iter(targets.into_iter().map(|(v, e)| (v, simplify_st(e))));
                    let otherwise = simplify_st(*otherwise);
                    SwitchTargets::SwitchInt(int_ty, targets, Box::new(otherwise))
                }
            };
            Statement::Switch(op, targets)
        }
        Statement::Loop(loop_body) => Statement::Loop(Box::new(simplify_st(*loop_body))),
        Statement::Sequence(st1, st2) => match *st2 {
            Statement::Sequence(st2, st3) => match *st3 {
                Statement::Sequence(st3, st4) => {
                    simplify_st_seq(*st1, *st2, *st3, Option::Some(*st4))
                }
                st3 => simplify_st_seq(*st1, *st2, st3, Option::None),
            },
            st2 => Statement::Sequence(Box::new(simplify_st(*st1)), Box::new(simplify_st(st2))),
        },
    }
}

fn simplify_def(mut def: FunDecl) -> FunDecl {
    trace!("# About to simplify: {}", def.name);
    def.body = match def.body {
        Option::Some(mut body) => {
            body.body = simplify_st(body.body);
            Option::Some(body)
        }
        Option::None => Option::None,
    };
    def
}

pub fn simplify(defs: FunDecls) -> FunDecls {
    FunDecls::from_iter(defs.into_iter().map(|def| simplify_def(def)))
}