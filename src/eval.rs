use crate::error::EvalError;
use crate::identifier::Ident;
use crate::operation::{continuate_operation, OperationCont};
use crate::position::RawSpan;
use crate::stack::Stack;
use crate::term::{RichTerm, Term};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

pub type Environment = HashMap<Ident, (Rc<RefCell<Closure>>, IdentKind)>;
pub type CallStack = Vec<StackElem>;

#[derive(Debug, PartialEq, Clone)]
pub enum StackElem {
    App(Option<RawSpan>),
    Var(IdentKind, Ident, Option<RawSpan>),
}

#[derive(Debug, PartialEq, Clone)]
pub enum IdentKind {
    Let(),
    Lam(),
    Record(),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Closure {
    pub body: RichTerm,
    pub env: Environment,
}

impl Closure {
    pub fn atomic_closure(body: RichTerm) -> Closure {
        Closure {
            body,
            env: HashMap::new(),
        }
    }
}

fn is_value(_term: &Term) -> bool {
    false
}

pub fn eval(t0: RichTerm) -> Result<Term, EvalError> {
    let mut clos = Closure::atomic_closure(t0);
    let mut call_stack = CallStack::new();
    let mut stack = Stack::new();
    let mut enriched_strict = true;

    loop {
        let Closure {
            body: RichTerm {
                term: boxed_term,
                pos,
            },
            mut env,
        } = clos;
        let term = *boxed_term;
        clos = match term {
            // Var
            Term::Var(x) => {
                let (thunk, id_kind) = env
                    .remove(&x)
                    .unwrap_or_else(|| panic!("Unbound variable {:?}", x));
                std::mem::drop(env); // thunk may be a 1RC pointer
                if !is_value(&thunk.borrow().body.term) {
                    stack.push_thunk(Rc::downgrade(&thunk));
                }
                call_stack.push(StackElem::Var(id_kind, x, pos));
                match Rc::try_unwrap(thunk) {
                    Ok(c) => {
                        // thunk was the only strong ref to the closure
                        c.into_inner()
                    }
                    Err(rc) => {
                        // We need to clone it, there are other strong refs
                        rc.borrow().clone()
                    }
                }
            }
            // App
            Term::App(t1, t2) => {
                stack.push_arg(
                    Closure {
                        body: t2,
                        env: env.clone(),
                    },
                    pos,
                );
                Closure { body: t1, env }
            }
            // Let
            Term::Let(x, s, t) => {
                let thunk = Rc::new(RefCell::new(Closure {
                    body: s,
                    env: env.clone(),
                }));
                env.insert(x, (Rc::clone(&thunk), IdentKind::Let()));
                Closure { body: t, env }
            }
            // Unary Operation
            Term::Op1(op, t) => {
                let op = op.map(|t| Closure {
                    body: t,
                    env: env.clone(),
                });

                stack.push_op_cont(OperationCont::Op1(op), call_stack.len(), pos);
                Closure { body: t, env }
            }
            // Binary Operation
            Term::Op2(op, fst, snd) => {
                let op = op.map(|t| Closure {
                    body: t,
                    env: env.clone(),
                });

                let prev_strict = enriched_strict;
                enriched_strict = op.is_strict();
                stack.push_op_cont(
                    OperationCont::Op2First(
                        op,
                        Closure {
                            body: snd,
                            env: env.clone(),
                        },
                        prev_strict,
                    ),
                    call_stack.len(),
                    pos,
                );
                Closure { body: fst, env }
            }
            // Promise and Assume
            Term::Promise(ty, l, t) | Term::Assume(ty, l, t) => {
                stack.push_arg(
                    Closure {
                        body: t,
                        env: env.clone(),
                    },
                    None,
                );
                stack.push_arg(Closure::atomic_closure(RichTerm::new(Term::Lbl(l))), None);
                Closure {
                    body: ty.contract(),
                    env,
                }
            }
            // Unwrap enriched terms
            Term::Contract(_, _) if enriched_strict => {
                return Err(EvalError::Other(
                    String::from(
                        "Expected a simple term, got a Contract. Contracts cannot be evaluated",
                    ),
                    pos,
                ));
            }
            Term::DefaultValue(t) | Term::Docstring(_, t) if enriched_strict => {
                Closure { body: t, env }
            }
            Term::ContractWithDefault(ty, label, t) if enriched_strict => Closure {
                body: Term::Assume(ty, label, t).into(),
                env,
            },
            // Continuate Operation
            // Update
            _ if 0 < stack.count_thunks() || 0 < stack.count_conts() => {
                clos = Closure {
                    body: RichTerm {
                        term: Box::new(term),
                        pos,
                    },
                    env,
                };
                if 0 < stack.count_thunks() {
                    while let Some(thunk) = stack.pop_thunk() {
                        if let Some(safe_thunk) = Weak::upgrade(&thunk) {
                            *safe_thunk.borrow_mut() = clos.clone();
                        }
                    }
                    clos
                } else {
                    let cont_result = continuate_operation(
                        clos,
                        &mut stack,
                        &mut call_stack,
                        &mut enriched_strict,
                    );

                    if let Err(EvalError::BlameError(l, _)) = cont_result {
                        return Err(EvalError::BlameError(l, Some(call_stack)));
                    }
                    cont_result?
                }
            }
            // Call
            Term::Fun(x, t) => {
                if 0 < stack.count_args() {
                    let (arg, pos) = stack.pop_arg().expect("Condition already checked.");
                    call_stack.push(StackElem::App(pos));
                    let thunk = Rc::new(RefCell::new(arg));
                    env.insert(x, (thunk, IdentKind::Lam()));
                    Closure { body: t, env }
                } else {
                    return Ok(Term::Fun(x, t));
                }
            }
            // Otherwise, this is either an ill-formed application, or we are done
            t => {
                if 0 < stack.count_args() {
                    let (arg, pos_app) = stack.pop_arg().expect("Condition already checked.");
                    return Err(EvalError::NotAFunc(
                        RichTerm {
                            term: Box::new(t),
                            pos,
                        },
                        arg.body,
                        pos_app,
                    ));
                } else {
                    return Ok(t);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::label::{Label, TyPath};
    use crate::term::{BinaryOp, UnaryOp};
    use crate::types::{AbsType, Types};
    use codespan::Files;

    fn mk_label() -> Label {
        Label {
            tag: "testing".to_string(),
            span: RawSpan {
                src_id: Files::new().add("<test>", String::from("empty")),
                start: 0.into(),
                end: 1.into(),
            },
            polarity: false,
            path: TyPath::Nil(),
        }
    }

    #[test]
    fn identity_over_values() {
        let num = Term::Num(45.3);
        assert_eq!(Ok(num.clone()), eval(num.into()));

        let boolean = Term::Bool(true);
        assert_eq!(Ok(boolean.clone()), eval(boolean.into()));

        let lambda = Term::Fun(
            Ident("x".to_string()),
            RichTerm::app(RichTerm::var("x".into()), RichTerm::var("x".into())),
        );
        assert_eq!(Ok(lambda.clone()), eval(lambda.into()));
    }

    #[test]
    fn blame_panics() {
        let label = mk_label();
        if let Err(EvalError::BlameError(l, _)) =
            eval(Term::Op1(UnaryOp::Blame(), Term::Lbl(label.clone()).into()).into())
        {
            assert_eq!(l, label);
        } else {
            panic!("This evaluation should've returned a BlameError!");
        }
    }

    #[test]
    #[should_panic]
    fn lone_var_panics() {
        eval(RichTerm::var("unbound".into())).unwrap();
    }

    #[test]
    fn only_fun_are_applicable() {
        eval(RichTerm::app(Term::Bool(true).into(), Term::Num(45.).into()).into()).unwrap_err();
    }

    #[test]
    fn simple_app() {
        let t = RichTerm::app(
            Term::Fun(Ident("x".to_string()), RichTerm::var("x".into())).into(),
            Term::Num(5.0).into(),
        );

        assert_eq!(Ok(Term::Num(5.0)), eval(t));
    }

    #[test]
    fn simple_let() {
        let t = RichTerm::let_in("x", Term::Num(5.0).into(), RichTerm::var("x".into()));

        assert_eq!(Ok(Term::Num(5.0)), eval(t));
    }

    #[test]
    fn simple_ite() {
        let t = RichTerm::ite(
            Term::Bool(true).into(),
            Term::Num(5.0).into(),
            Term::Bool(false).into(),
        );

        assert_eq!(Ok(Term::Num(5.0)), eval(t));
    }

    #[test]
    fn simple_plus() {
        let t = RichTerm::plus(Term::Num(5.0).into(), Term::Num(7.5).into());

        assert_eq!(Ok(Term::Num(12.5)), eval(t));
    }

    #[test]
    fn simple_is_zero() {
        let t = Term::Op1(UnaryOp::IsZero(), Term::Num(7.0).into()).into();

        assert_eq!(Ok(Term::Bool(false)), eval(t));
    }

    #[test]
    fn asking_for_various_types() {
        let num = Term::Op1(UnaryOp::IsNum(), Term::Num(45.3).into()).into();
        assert_eq!(Ok(Term::Bool(true)), eval(num));

        let boolean = Term::Op1(UnaryOp::IsBool(), Term::Bool(true).into()).into();
        assert_eq!(Ok(Term::Bool(true)), eval(boolean));

        let lambda = Term::Op1(
            UnaryOp::IsFun(),
            Term::Fun(
                Ident("x".to_string()),
                RichTerm::app(RichTerm::var("x".into()), RichTerm::var("x".into())),
            )
            .into(),
        )
        .into();
        assert_eq!(Ok(Term::Bool(true)), eval(lambda));
    }

    #[test]
    fn enriched_terms_unwrapping() {
        let t = Term::DefaultValue(
            Term::DefaultValue(Term::Docstring("a".to_string(), Term::Bool(false).into()).into())
                .into(),
        )
        .into();
        assert_eq!(Ok(Term::Bool(false)), eval(t));
    }

    #[test]
    fn merge_enriched_default() {
        let t = Term::Op2(
            BinaryOp::Merge(),
            Term::Num(1.0).into(),
            Term::DefaultValue(Term::Num(2.0).into()).into(),
        )
        .into();
        assert_eq!(Ok(Term::Num(1.0)), eval(t));
    }

    #[test]
    fn merge_multiple_defaults() {
        let t = Term::Op2(
            BinaryOp::Merge(),
            Term::DefaultValue(Term::Num(1.0).into()).into(),
            Term::DefaultValue(Term::Num(2.0).into()).into(),
        )
        .into();

        eval(t).unwrap_err();

        let t = Term::Op2(
            BinaryOp::Merge(),
            Term::ContractWithDefault(Types(AbsType::Num()), mk_label(), Term::Num(1.0).into())
                .into(),
            Term::DefaultValue(Term::Num(2.0).into()).into(),
        )
        .into();

        eval(t).unwrap_err();
    }
}
