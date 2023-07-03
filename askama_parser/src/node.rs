use std::str;

use nom::branch::alt;
use nom::bytes::complete::{tag, take_until};
use nom::character::complete::char;
use nom::combinator::{complete, consumed, cut, eof, map, not, opt, peek, recognize, value};
use nom::error::{Error, ErrorKind};
use nom::multi::{fold_many0, many0, many1, separated_list0, separated_list1};
use nom::sequence::{delimited, pair, preceded, terminated, tuple};
use nom::{error_position, IResult};

use super::{
    bool_lit, char_lit, identifier, keyword, num_lit, path, skip_till, split_ws_parts, str_lit, ws,
    Expr, State,
};

#[derive(Debug, PartialEq)]
pub enum Node<'a> {
    Lit(&'a str, &'a str, &'a str),
    Comment(Ws),
    Expr(Ws, Expr<'a>),
    Call(Ws, Option<&'a str>, &'a str, Vec<Expr<'a>>),
    LetDecl(Ws, Target<'a>),
    Let(Ws, Target<'a>, Expr<'a>),
    Cond(Vec<Cond<'a>>, Ws),
    Match(Ws, Expr<'a>, Vec<When<'a>>, Ws),
    Loop(Loop<'a>),
    Extends(&'a str),
    BlockDef(Ws, &'a str, Vec<Node<'a>>, Ws),
    Include(Ws, &'a str),
    Import(Ws, &'a str, &'a str),
    Macro(&'a str, Macro<'a>),
    Raw(Ws, &'a str, &'a str, &'a str, Ws),
    Break(Ws),
    Continue(Ws),
}

impl<'a> Node<'a> {
    pub(super) fn many(i: &'a str, s: &State<'_>) -> IResult<&'a str, Vec<Self>> {
        many0(alt((
            complete(|i| Self::content(i, s)),
            complete(|i| Self::comment(i, s)),
            complete(|i| Self::expr(i, s)),
            complete(|i| Self::parse(i, s)),
        )))(i)
    }

    fn content(i: &'a str, s: &State<'_>) -> IResult<&'a str, Self> {
        let p_start = alt((
            tag(s.syntax.block_start),
            tag(s.syntax.comment_start),
            tag(s.syntax.expr_start),
        ));

        let (i, _) = not(eof)(i)?;
        let (i, content) = opt(recognize(skip_till(p_start)))(i)?;
        let (i, content) = match content {
            Some("") => {
                // {block,comment,expr}_start follows immediately.
                return Err(nom::Err::Error(error_position!(i, ErrorKind::TakeUntil)));
            }
            Some(content) => (i, content),
            None => ("", i), // there is no {block,comment,expr}_start: take everything
        };
        Ok((i, split_ws_parts(content)))
    }

    fn parse(i: &'a str, s: &State<'_>) -> IResult<&'a str, Self> {
        let mut p = tuple((
            |i| s.tag_block_start(i),
            alt((
                Self::call,
                Self::r#let,
                |i| Self::r#if(i, s),
                |i| Self::r#for(i, s),
                |i| Self::r#match(i, s),
                Self::extends,
                Self::include,
                Self::import,
                |i| Self::block(i, s),
                |i| Self::r#macro(i, s),
                |i| Self::raw(i, s),
                |i| Self::r#break(i, s),
                |i| Self::r#continue(i, s),
            )),
            cut(|i| s.tag_block_end(i)),
        ));
        let (i, (_, contents, _)) = p(i)?;
        Ok((i, contents))
    }

    fn call(i: &'a str) -> IResult<&'a str, Self> {
        let mut p = tuple((
            opt(Whitespace::parse),
            ws(keyword("call")),
            cut(tuple((
                opt(tuple((ws(identifier), ws(tag("::"))))),
                ws(identifier),
                opt(ws(Expr::parse_arguments)),
                opt(Whitespace::parse),
            ))),
        ));
        let (i, (pws, _, (scope, name, args, nws))) = p(i)?;
        let scope = scope.map(|(scope, _)| scope);
        let args = args.unwrap_or_default();
        Ok((i, Self::Call(Ws(pws, nws), scope, name, args)))
    }

    fn r#let(i: &'a str) -> IResult<&'a str, Self> {
        let mut p = tuple((
            opt(Whitespace::parse),
            ws(alt((keyword("let"), keyword("set")))),
            cut(tuple((
                ws(Target::parse),
                opt(tuple((ws(char('=')), ws(Expr::parse)))),
                opt(Whitespace::parse),
            ))),
        ));
        let (i, (pws, _, (var, val, nws))) = p(i)?;

        Ok((
            i,
            if let Some((_, val)) = val {
                Self::Let(Ws(pws, nws), var, val)
            } else {
                Self::LetDecl(Ws(pws, nws), var)
            },
        ))
    }

    fn r#if(i: &'a str, s: &State<'_>) -> IResult<&'a str, Self> {
        let mut p = tuple((
            opt(Whitespace::parse),
            CondTest::parse,
            cut(tuple((
                opt(Whitespace::parse),
                |i| s.tag_block_end(i),
                cut(tuple((
                    |i| Node::many(i, s),
                    many0(|i| Cond::parse(i, s)),
                    cut(tuple((
                        |i| s.tag_block_start(i),
                        opt(Whitespace::parse),
                        ws(keyword("endif")),
                        opt(Whitespace::parse),
                    ))),
                ))),
            ))),
        ));
        let (i, (pws1, cond, (nws1, _, (block, elifs, (_, pws2, _, nws2))))) = p(i)?;

        let mut res = vec![Cond {
            ws: Ws(pws1, nws1),
            cond: Some(cond),
            block,
        }];
        res.extend(elifs);
        Ok((i, Self::Cond(res, Ws(pws2, nws2))))
    }

    fn r#for(i: &'a str, s: &State<'_>) -> IResult<&'a str, Self> {
        fn content<'a>(i: &'a str, s: &State<'_>) -> IResult<&'a str, Vec<Node<'a>>> {
            s.enter_loop();
            let result = Node::many(i, s);
            s.leave_loop();
            result
        }

        let if_cond = preceded(ws(keyword("if")), cut(ws(Expr::parse)));
        let else_block = |i| {
            let mut p = preceded(
                ws(keyword("else")),
                cut(tuple((
                    opt(Whitespace::parse),
                    delimited(
                        |i| s.tag_block_end(i),
                        |i| Self::many(i, s),
                        |i| s.tag_block_start(i),
                    ),
                    opt(Whitespace::parse),
                ))),
            );
            let (i, (pws, nodes, nws)) = p(i)?;
            Ok((i, (pws, nodes, nws)))
        };
        let mut p = tuple((
            opt(Whitespace::parse),
            ws(keyword("for")),
            cut(tuple((
                ws(Target::parse),
                ws(keyword("in")),
                cut(tuple((
                    ws(Expr::parse),
                    opt(if_cond),
                    opt(Whitespace::parse),
                    |i| s.tag_block_end(i),
                    cut(tuple((
                        |i| content(i, s),
                        cut(tuple((
                            |i| s.tag_block_start(i),
                            opt(Whitespace::parse),
                            opt(else_block),
                            ws(keyword("endfor")),
                            opt(Whitespace::parse),
                        ))),
                    ))),
                ))),
            ))),
        ));
        let (i, (pws1, _, (var, _, (iter, cond, nws1, _, (body, (_, pws2, else_block, _, nws2)))))) =
            p(i)?;
        let (nws3, else_block, pws3) = else_block.unwrap_or_default();
        Ok((
            i,
            Self::Loop(Loop {
                ws1: Ws(pws1, nws1),
                var,
                iter,
                cond,
                body,
                ws2: Ws(pws2, nws3),
                else_block,
                ws3: Ws(pws3, nws2),
            }),
        ))
    }

    fn r#match(i: &'a str, s: &State<'_>) -> IResult<&'a str, Self> {
        let mut p = tuple((
            opt(Whitespace::parse),
            ws(keyword("match")),
            cut(tuple((
                ws(Expr::parse),
                opt(Whitespace::parse),
                |i| s.tag_block_end(i),
                cut(tuple((
                    ws(many0(ws(value((), |i| Self::comment(i, s))))),
                    many1(|i| When::when(i, s)),
                    cut(tuple((
                        opt(|i| When::r#match(i, s)),
                        cut(tuple((
                            ws(|i| s.tag_block_start(i)),
                            opt(Whitespace::parse),
                            ws(keyword("endmatch")),
                            opt(Whitespace::parse),
                        ))),
                    ))),
                ))),
            ))),
        ));
        let (i, (pws1, _, (expr, nws1, _, (_, arms, (else_arm, (_, pws2, _, nws2)))))) = p(i)?;

        let mut arms = arms;
        if let Some(arm) = else_arm {
            arms.push(arm);
        }

        Ok((i, Self::Match(Ws(pws1, nws1), expr, arms, Ws(pws2, nws2))))
    }

    fn extends(i: &'a str) -> IResult<&'a str, Self> {
        let (i, (_, name)) = tuple((ws(keyword("extends")), ws(str_lit)))(i)?;
        Ok((i, Self::Extends(name)))
    }

    fn include(i: &'a str) -> IResult<&'a str, Self> {
        let mut p = tuple((
            opt(Whitespace::parse),
            ws(keyword("include")),
            cut(pair(ws(str_lit), opt(Whitespace::parse))),
        ));
        let (i, (pws, _, (name, nws))) = p(i)?;
        Ok((i, Self::Include(Ws(pws, nws), name)))
    }

    fn block(i: &'a str, s: &State<'_>) -> IResult<&'a str, Self> {
        let mut start = tuple((
            opt(Whitespace::parse),
            ws(keyword("block")),
            cut(tuple((ws(identifier), opt(Whitespace::parse), |i| {
                s.tag_block_end(i)
            }))),
        ));
        let (i, (pws1, _, (name, nws1, _))) = start(i)?;

        let mut end = cut(tuple((
            |i| Self::many(i, s),
            cut(tuple((
                |i| s.tag_block_start(i),
                opt(Whitespace::parse),
                ws(keyword("endblock")),
                cut(tuple((opt(ws(keyword(name))), opt(Whitespace::parse)))),
            ))),
        )));
        let (i, (contents, (_, pws2, _, (_, nws2)))) = end(i)?;

        Ok((
            i,
            Self::BlockDef(Ws(pws1, nws1), name, contents, Ws(pws2, nws2)),
        ))
    }

    fn import(i: &'a str) -> IResult<&'a str, Self> {
        let mut p = tuple((
            opt(Whitespace::parse),
            ws(keyword("import")),
            cut(tuple((
                ws(str_lit),
                ws(keyword("as")),
                cut(pair(ws(identifier), opt(Whitespace::parse))),
            ))),
        ));
        let (i, (pws, _, (name, _, (scope, nws)))) = p(i)?;
        Ok((i, Self::Import(Ws(pws, nws), name, scope)))
    }

    fn r#macro(i: &'a str, s: &State<'_>) -> IResult<&'a str, Self> {
        fn parameters(i: &str) -> IResult<&str, Vec<&str>> {
            delimited(
                ws(char('(')),
                separated_list0(char(','), ws(identifier)),
                ws(char(')')),
            )(i)
        }

        let mut start = tuple((
            opt(Whitespace::parse),
            ws(keyword("macro")),
            cut(tuple((
                ws(identifier),
                opt(ws(parameters)),
                opt(Whitespace::parse),
                |i| s.tag_block_end(i),
            ))),
        ));
        let (i, (pws1, _, (name, params, nws1, _))) = start(i)?;

        let mut end = cut(tuple((
            |i| Self::many(i, s),
            cut(tuple((
                |i| s.tag_block_start(i),
                opt(Whitespace::parse),
                ws(keyword("endmacro")),
                cut(tuple((opt(ws(keyword(name))), opt(Whitespace::parse)))),
            ))),
        )));
        let (i, (contents, (_, pws2, _, (_, nws2)))) = end(i)?;

        assert_ne!(name, "super", "invalid macro name 'super'");

        let params = params.unwrap_or_default();

        Ok((
            i,
            Self::Macro(
                name,
                Macro {
                    ws1: Ws(pws1, nws1),
                    args: params,
                    nodes: contents,
                    ws2: Ws(pws2, nws2),
                },
            ),
        ))
    }

    fn raw(i: &'a str, s: &State<'_>) -> IResult<&'a str, Self> {
        let endraw = tuple((
            |i| s.tag_block_start(i),
            opt(Whitespace::parse),
            ws(keyword("endraw")),
            opt(Whitespace::parse),
            peek(|i| s.tag_block_end(i)),
        ));

        let mut p = tuple((
            opt(Whitespace::parse),
            ws(keyword("raw")),
            cut(tuple((
                opt(Whitespace::parse),
                |i| s.tag_block_end(i),
                consumed(skip_till(endraw)),
            ))),
        ));

        let (_, (pws1, _, (nws1, _, (contents, (i, (_, pws2, _, nws2, _)))))) = p(i)?;
        let (lws, val, rws) = match split_ws_parts(contents) {
            Node::Lit(lws, val, rws) => (lws, val, rws),
            _ => unreachable!(),
        };
        let ws1 = Ws(pws1, nws1);
        let ws2 = Ws(pws2, nws2);
        Ok((i, Self::Raw(ws1, lws, val, rws, ws2)))
    }

    fn r#break(i: &'a str, s: &State<'_>) -> IResult<&'a str, Self> {
        let mut p = tuple((
            opt(Whitespace::parse),
            ws(keyword("break")),
            opt(Whitespace::parse),
        ));
        let (j, (pws, _, nws)) = p(i)?;
        if !s.is_in_loop() {
            return Err(nom::Err::Failure(error_position!(i, ErrorKind::Tag)));
        }
        Ok((j, Self::Break(Ws(pws, nws))))
    }

    fn r#continue(i: &'a str, s: &State<'_>) -> IResult<&'a str, Self> {
        let mut p = tuple((
            opt(Whitespace::parse),
            ws(keyword("continue")),
            opt(Whitespace::parse),
        ));
        let (j, (pws, _, nws)) = p(i)?;
        if !s.is_in_loop() {
            return Err(nom::Err::Failure(error_position!(i, ErrorKind::Tag)));
        }
        Ok((j, Self::Continue(Ws(pws, nws))))
    }

    fn expr(i: &'a str, s: &State<'_>) -> IResult<&'a str, Self> {
        let mut p = tuple((
            |i| s.tag_expr_start(i),
            cut(tuple((
                opt(Whitespace::parse),
                ws(Expr::parse),
                opt(Whitespace::parse),
                |i| s.tag_expr_end(i),
            ))),
        ));
        let (i, (_, (pws, expr, nws, _))) = p(i)?;
        Ok((i, Self::Expr(Ws(pws, nws), expr)))
    }

    fn comment(i: &'a str, s: &State<'_>) -> IResult<&'a str, Self> {
        fn body<'a>(mut i: &'a str, s: &State<'_>) -> IResult<&'a str, &'a str> {
            let mut level = 0;
            loop {
                let (end, tail) = take_until(s.syntax.comment_end)(i)?;
                match take_until::<_, _, Error<_>>(s.syntax.comment_start)(i) {
                    Ok((start, _)) if start.as_ptr() < end.as_ptr() => {
                        level += 1;
                        i = &start[2..];
                    }
                    _ if level > 0 => {
                        level -= 1;
                        i = &end[2..];
                    }
                    _ => return Ok((end, tail)),
                }
            }
        }

        let mut p = tuple((
            |i| s.tag_comment_start(i),
            cut(tuple((
                opt(Whitespace::parse),
                |i| body(i, s),
                |i| s.tag_comment_end(i),
            ))),
        ));
        let (i, (_, (pws, tail, _))) = p(i)?;
        let nws = if tail.ends_with('-') {
            Some(Whitespace::Suppress)
        } else if tail.ends_with('+') {
            Some(Whitespace::Preserve)
        } else if tail.ends_with('~') {
            Some(Whitespace::Minimize)
        } else {
            None
        };
        Ok((i, Self::Comment(Ws(pws, nws))))
    }
}

#[derive(Debug, PartialEq)]
pub enum Target<'a> {
    Name(&'a str),
    Tuple(Vec<&'a str>, Vec<Target<'a>>),
    Struct(Vec<&'a str>, Vec<(&'a str, Target<'a>)>),
    NumLit(&'a str),
    StrLit(&'a str),
    CharLit(&'a str),
    BoolLit(&'a str),
    Path(Vec<&'a str>),
}

impl<'a> Target<'a> {
    pub(super) fn parse(i: &'a str) -> IResult<&'a str, Self> {
        let mut opt_opening_paren = map(opt(ws(char('('))), |o| o.is_some());
        let mut opt_closing_paren = map(opt(ws(char(')'))), |o| o.is_some());
        let mut opt_opening_brace = map(opt(ws(char('{'))), |o| o.is_some());

        let (i, lit) = opt(Self::lit)(i)?;
        if let Some(lit) = lit {
            return Ok((i, lit));
        }

        // match tuples and unused parentheses
        let (i, target_is_tuple) = opt_opening_paren(i)?;
        if target_is_tuple {
            let (i, is_empty_tuple) = opt_closing_paren(i)?;
            if is_empty_tuple {
                return Ok((i, Self::Tuple(Vec::new(), Vec::new())));
            }

            let (i, first_target) = Self::parse(i)?;
            let (i, is_unused_paren) = opt_closing_paren(i)?;
            if is_unused_paren {
                return Ok((i, first_target));
            }

            let mut targets = vec![first_target];
            let (i, _) = cut(tuple((
                fold_many0(
                    preceded(ws(char(',')), Self::parse),
                    || (),
                    |_, target| {
                        targets.push(target);
                    },
                ),
                opt(ws(char(','))),
                ws(cut(char(')'))),
            )))(i)?;
            return Ok((i, Self::Tuple(Vec::new(), targets)));
        }

        // match structs
        let (i, path) = opt(path)(i)?;
        if let Some(path) = path {
            let i_before_matching_with = i;
            let (i, _) = opt(ws(keyword("with")))(i)?;

            let (i, is_unnamed_struct) = opt_opening_paren(i)?;
            if is_unnamed_struct {
                let (i, targets) = alt((
                    map(char(')'), |_| Vec::new()),
                    terminated(
                        cut(separated_list1(ws(char(',')), Self::parse)),
                        pair(opt(ws(char(','))), ws(cut(char(')')))),
                    ),
                ))(i)?;
                return Ok((i, Self::Tuple(path, targets)));
            }

            let (i, is_named_struct) = opt_opening_brace(i)?;
            if is_named_struct {
                let (i, targets) = alt((
                    map(char('}'), |_| Vec::new()),
                    terminated(
                        cut(separated_list1(ws(char(',')), Self::named)),
                        pair(opt(ws(char(','))), ws(cut(char('}')))),
                    ),
                ))(i)?;
                return Ok((i, Self::Struct(path, targets)));
            }

            return Ok((i_before_matching_with, Self::Path(path)));
        }

        // neither literal nor struct nor path
        map(identifier, Self::Name)(i)
    }

    fn lit(i: &'a str) -> IResult<&'a str, Self> {
        alt((
            map(str_lit, Self::StrLit),
            map(char_lit, Self::CharLit),
            map(num_lit, Self::NumLit),
            map(bool_lit, Self::BoolLit),
        ))(i)
    }

    fn named(i: &'a str) -> IResult<&str, (&str, Self)> {
        let (i, (src, target)) = pair(identifier, opt(preceded(ws(char(':')), Self::parse)))(i)?;
        Ok((i, (src, target.unwrap_or(Self::Name(src)))))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Whitespace {
    Preserve,
    Suppress,
    Minimize,
}

impl Whitespace {
    fn parse(i: &str) -> IResult<&str, Self> {
        alt((char('-'), char('+'), char('~')))(i).map(|(s, r)| (s, Self::from(r)))
    }
}

impl From<char> for Whitespace {
    fn from(c: char) -> Self {
        match c {
            '+' => Self::Preserve,
            '-' => Self::Suppress,
            '~' => Self::Minimize,
            _ => panic!("unsupported `Whitespace` conversion"),
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct Loop<'a> {
    pub ws1: Ws,
    pub var: Target<'a>,
    pub iter: Expr<'a>,
    pub cond: Option<Expr<'a>>,
    pub body: Vec<Node<'a>>,
    pub ws2: Ws,
    pub else_block: Vec<Node<'a>>,
    pub ws3: Ws,
}

#[derive(Debug, PartialEq)]
pub struct When<'a> {
    pub ws: Ws,
    pub target: Target<'a>,
    pub block: Vec<Node<'a>>,
}

impl<'a> When<'a> {
    fn r#match(i: &'a str, s: &State<'_>) -> IResult<&'a str, Self> {
        let mut p = tuple((
            |i| s.tag_block_start(i),
            opt(Whitespace::parse),
            ws(keyword("else")),
            cut(tuple((
                opt(Whitespace::parse),
                |i| s.tag_block_end(i),
                cut(|i| Node::many(i, s)),
            ))),
        ));
        let (i, (_, pws, _, (nws, _, block))) = p(i)?;
        Ok((
            i,
            Self {
                ws: Ws(pws, nws),
                target: Target::Name("_"),
                block,
            },
        ))
    }

    #[allow(clippy::self_named_constructors)]
    fn when(i: &'a str, s: &State<'_>) -> IResult<&'a str, Self> {
        let mut p = tuple((
            |i| s.tag_block_start(i),
            opt(Whitespace::parse),
            ws(keyword("when")),
            cut(tuple((
                ws(Target::parse),
                opt(Whitespace::parse),
                |i| s.tag_block_end(i),
                cut(|i| Node::many(i, s)),
            ))),
        ));
        let (i, (_, pws, _, (target, nws, _, block))) = p(i)?;
        Ok((
            i,
            Self {
                ws: Ws(pws, nws),
                target,
                block,
            },
        ))
    }
}

#[derive(Debug, PartialEq)]
pub struct Macro<'a> {
    pub ws1: Ws,
    pub args: Vec<&'a str>,
    pub nodes: Vec<Node<'a>>,
    pub ws2: Ws,
}

/// First field is "minus/plus sign was used on the left part of the item".
///
/// Second field is "minus/plus sign was used on the right part of the item".
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ws(pub Option<Whitespace>, pub Option<Whitespace>);

#[derive(Debug, PartialEq)]
pub struct Cond<'a> {
    pub ws: Ws,
    pub cond: Option<CondTest<'a>>,
    pub block: Vec<Node<'a>>,
}

impl<'a> Cond<'a> {
    fn parse(i: &'a str, s: &State<'_>) -> IResult<&'a str, Self> {
        let mut p = tuple((
            |i| s.tag_block_start(i),
            opt(Whitespace::parse),
            ws(keyword("else")),
            cut(tuple((
                opt(CondTest::parse),
                opt(Whitespace::parse),
                |i| s.tag_block_end(i),
                cut(|i| Node::many(i, s)),
            ))),
        ));
        let (i, (_, pws, _, (cond, nws, _, block))) = p(i)?;
        Ok((
            i,
            Cond {
                ws: Ws(pws, nws),
                cond,
                block,
            },
        ))
    }
}

#[derive(Debug, PartialEq)]
pub struct CondTest<'a> {
    pub target: Option<Target<'a>>,
    pub expr: Expr<'a>,
}

impl<'a> CondTest<'a> {
    fn parse(i: &'a str) -> IResult<&'a str, Self> {
        let mut p = preceded(
            ws(keyword("if")),
            cut(tuple((
                opt(delimited(
                    ws(alt((keyword("let"), keyword("set")))),
                    ws(Target::parse),
                    ws(char('=')),
                )),
                ws(Expr::parse),
            ))),
        );
        let (i, (target, expr)) = p(i)?;
        Ok((i, CondTest { target, expr }))
    }
}
