//! Fact extraction for Rust, JavaScript, TypeScript and C#.

use oi_lang::{CSharpExtractor, Extractor, RustExtractor, ScriptExtractor};
use oi_model::{FileFacts, RefKind, SymbolKind};

fn sym<'a>(f: &'a FileFacts, name: &str, kind: SymbolKind) -> &'a oi_model::Symbol {
    f.symbols
        .iter()
        .find(|s| s.name == name && s.kind == kind)
        .unwrap_or_else(|| panic!("no {kind:?} {name} in {:#?}", f.symbols))
}

fn bases(f: &FileFacts) -> Vec<(String, String)> {
    f.bases
        .iter()
        .map(|b| (b.derived.clone(), b.base_written.clone()))
        .collect()
}

fn call<'a>(f: &'a FileFacts, name: &str) -> &'a oi_model::Reference {
    f.references
        .iter()
        .find(|r| r.name == name && r.kind == RefKind::Call)
        .unwrap_or_else(|| panic!("no call {name} in {:#?}", f.references))
}

fn imports(f: &FileFacts) -> Vec<(String, bool)> {
    f.includes
        .iter()
        .map(|i| (i.spec.clone(), i.module))
        .collect()
}

#[test]
fn rust() {
    let src = r#"use std::{fmt, collections::HashMap as Map, io::{self, Write}};
use crate::store::Store;

pub mod net {
    pub trait Listener {
        fn on_start(&mut self, req: &Request) -> bool;
        fn name(&self) -> &str { "x" }
    }

    pub struct Channel<T> { count: u32, inner: T }

    impl<T> Listener for Channel<T> {
        fn on_start(&mut self, req: &Request) -> bool {
            helper(req);
            self.inner.poll();
            let n = ctx.find_node::<ast::Loop>()?;
            let m = parse::<u8>(s);
            let v = vec![ctx.find_in_macro::<Either<A, B>>()?, plain(1), inner!(x)];
            log!("started");
            true
        }
    }

    impl<T> Channel<T> {
        pub fn new(inner: T) -> Self { Channel { count: 0, inner } }
    }

    enum State { Idle, Busy(u8) }
}

macro_rules! log { ($e:expr) => {} }
fn helper(r: &Request) {}
"#;
    let f = RustExtractor.extract(src.as_bytes()).unwrap();
    assert_eq!(
        imports(&f),
        [
            ("std::fmt".into(), true),
            ("std::collections::HashMap".into(), true),
            ("std::io".into(), true),
            ("std::io::Write".into(), true),
            ("crate::store::Store".into(), true)
        ]
    );
    assert_eq!(sym(&f, "net", SymbolKind::Namespace).scope, "");
    let tr = sym(&f, "Listener", SymbolKind::Interface);
    assert_eq!(tr.scope, "net");
    assert_eq!(sym(&f, "Channel", SymbolKind::Struct).scope, "net");
    assert_eq!(sym(&f, "count", SymbolKind::Field).scope, "net::Channel");
    assert_eq!(sym(&f, "Busy", SymbolKind::Enumerator).scope, "net::State");
    assert_eq!(sym(&f, "log", SymbolKind::Macro).line, 31);
    for name in ["find_in_macro", "plain", "inner"] {
        assert_eq!(call(&f, name).line, 18, "{name}");
    }
    assert!(
        !f.references
            .iter()
            .any(|r| r.name == "Either" && r.kind == RefKind::Call)
    );
    assert_eq!(call(&f, "find_node").line, 16);
    assert_eq!(call(&f, "parse").line, 17);
    assert_eq!(bases(&f), [("Channel".into(), "Listener".into())]);

    let impls: Vec<_> = f
        .symbols
        .iter()
        .filter(|s| s.name == "on_start")
        .map(|s| {
            (
                s.scope.as_str(),
                s.is_definition,
                s.is_virtual,
                s.signature.as_str(),
            )
        })
        .collect();
    assert_eq!(
        impls,
        [
            ("net::Listener", false, true, "(&mut self, &Request)"),
            ("net::Channel", true, true, "(&mut self, &Request)")
        ]
    );
    let new = sym(&f, "new", SymbolKind::Method);
    assert_eq!(
        (new.scope.as_str(), new.is_virtual),
        ("net::Channel", false)
    );
    assert_eq!(sym(&f, "helper", SymbolKind::Function).scope, "");

    assert_eq!(
        call(&f, "helper").container.as_deref(),
        Some("net::Channel::on_start")
    );
    assert_eq!(call(&f, "poll").line, 15);
    assert_eq!(call(&f, "log").line, 19);
    assert!(
        f.references
            .iter()
            .any(|r| r.name == "Request" && r.kind == RefKind::Type)
    );
    assert_eq!(f.parse_errors, 0);
}

#[test]
fn rust_token_tree_calls() {
    let src = r"#[cfg(any(unix, windows))]
#![cfg_attr(test, derive(Debug))]
macro_rules! m {
    ($($x:expr),*) => {
        fn defined(x: u8) {}
        struct W(u8);
        $( repeated($x); )*
        $f(1);
    };
}
fn main() {
    foo!(x.g::<<T as Tr>::A>(1));
}
";
    let f = RustExtractor.extract(src.as_bytes()).unwrap();
    let calls: Vec<_> = f
        .references
        .iter()
        .filter(|r| r.kind == RefKind::Call)
        .map(|r| (r.name.as_str(), r.line))
        .collect();
    assert_eq!(
        calls,
        [("repeated", 7), ("foo", 12), ("g", 12)],
        "{:#?}",
        f.references
    );
}

#[test]
fn typescript() {
    let src = r#"import { Component } from "react";
import { util } from "./util";
export * from "../shared/types";
const fs = require("fs");

export interface Listener extends Base<number> {
  onStart(req: Request): boolean;
  name: string;
}

export abstract class Channel extends Emitter implements Listener, ns.Other {
  private count = 0;
  onStart(req: Request): boolean {
    helper(req);
    this.emit("start");
    return new Response(req).ok;
  }
}

namespace net {
  export enum State { Idle, Busy }
}

export const helper = (r: Request): void => { util.log(r); };
type Alias = Channel | null;
"#;
    let ts = ScriptExtractor::typescript();
    let f = ts.extract_path("src/channel.ts", src.as_bytes()).unwrap();
    assert_eq!(
        imports(&f),
        [
            ("react".into(), true),
            ("./util".into(), false),
            ("../shared/types".into(), false),
            ("fs".into(), true)
        ]
    );
    assert_eq!(sym(&f, "Listener", SymbolKind::Interface).line, 6);
    assert_eq!(sym(&f, "Channel", SymbolKind::Class).line, 11);
    assert_eq!(sym(&f, "count", SymbolKind::Property).scope, "Channel");
    assert_eq!(sym(&f, "name", SymbolKind::Property).scope, "Listener");
    assert_eq!(sym(&f, "State", SymbolKind::Enum).scope, "net");
    assert_eq!(sym(&f, "Alias", SymbolKind::Typedef).line, 25);
    assert_eq!(
        bases(&f),
        [
            ("Listener".into(), "Base".into()),
            ("Channel".into(), "Emitter".into()),
            ("Channel".into(), "Listener".into()),
            ("Channel".into(), "ns::Other".into())
        ]
    );
    let methods: Vec<_> = f
        .symbols
        .iter()
        .filter(|s| s.name == "onStart")
        .map(|s| (s.scope.as_str(), s.is_definition, s.signature.as_str()))
        .collect();
    assert_eq!(
        methods,
        [
            ("Listener", false, "(Request)"),
            ("Channel", true, "(Request)")
        ]
    );
    let helper = sym(&f, "helper", SymbolKind::Function);
    assert_eq!((helper.line, helper.signature.as_str()), (24, "(Request)"));
    assert_eq!(
        call(&f, "helper").container.as_deref(),
        Some("Channel::onStart")
    );
    assert_eq!(call(&f, "emit").line, 15);
    assert_eq!(call(&f, "Response").line, 16);
    assert_eq!(call(&f, "log").container.as_deref(), Some("helper"));
    assert_eq!(f.parse_errors, 0);

    // Locals are not symbols.
    let local = "function f() { const x = 1; const g = () => 2; }\n";
    let f = ts.extract_path("a.ts", local.as_bytes()).unwrap();
    assert!(f.symbols.iter().all(|s| s.name == "f"), "{:#?}", f.symbols);

    // TSX parses JSX.
    let tsx = "export function App(): JSX.Element { return <div onClick={() => go()} />; }\n";
    let f = ts.extract_path("a.tsx", tsx.as_bytes()).unwrap();
    assert_eq!(f.parse_errors, 0);
    assert_eq!(call(&f, "go").container.as_deref(), Some("App"));
}

#[test]
fn javascript() {
    let src = "import x from './x.js';\nclass A extends B.C { m() { return super.m(); } }\nfunction run() { new A().m(); }\n";
    let js = ScriptExtractor::javascript();
    assert!(js.handles("a/b.mjs") && !js.handles("a/b.ts"));
    let f = js.extract_path("a.js", src.as_bytes()).unwrap();
    assert_eq!(imports(&f), [("./x.js".into(), false)]);
    assert_eq!(bases(&f), [("A".into(), "B::C".into())]);
    assert_eq!(sym(&f, "m", SymbolKind::Method).scope, "A");
    assert_eq!(call(&f, "A").container.as_deref(), Some("run"));
}

#[test]
fn csharp() {
    let src = r"using System;
using System.Collections.Generic;
using Alias = Foo.Bar.Baz;

namespace Acme.Net
{
    public interface IListener { bool OnStart(Request req); }

    public class Channel : Emitter<int>, IListener, System.IDisposable
    {
        private int count, other;
        public string Name { get; set; }
        public Channel(int c) { count = c; }
        public virtual bool OnStart(Request req) { Helper(req); return Run<int>(1); }
        public override string ToString() => base.ToString();
        public void Dispose() { var x = new Response(); log.Write(x); }
    }

    public enum State { Idle, Busy }
    public record Point(int X, int Y) : Shape(X);
}
";
    let f = CSharpExtractor.extract(src.as_bytes()).unwrap();
    assert_eq!(
        imports(&f),
        [
            ("System".into(), true),
            ("System.Collections.Generic".into(), true),
            ("Foo.Bar.Baz".into(), true)
        ]
    );
    assert_eq!(sym(&f, "Net", SymbolKind::Namespace).scope, "Acme");
    assert_eq!(
        sym(&f, "IListener", SymbolKind::Interface).scope,
        "Acme::Net"
    );
    assert_eq!(sym(&f, "Channel", SymbolKind::Class).scope, "Acme::Net");
    assert_eq!(
        sym(&f, "other", SymbolKind::Field).scope,
        "Acme::Net::Channel"
    );
    assert_eq!(sym(&f, "Name", SymbolKind::Property).line, 12);
    assert_eq!(
        sym(&f, "Busy", SymbolKind::Enumerator).scope,
        "Acme::Net::State"
    );
    assert_eq!(sym(&f, "Point", SymbolKind::Class).line, 20);
    assert_eq!(
        bases(&f),
        [
            ("Channel".into(), "Emitter".into()),
            ("Channel".into(), "IListener".into()),
            ("Channel".into(), "System::IDisposable".into()),
            ("Point".into(), "Shape".into())
        ]
    );
    let on_start: Vec<_> = f
        .symbols
        .iter()
        .filter(|s| s.name == "OnStart")
        .map(|s| {
            (
                s.scope.as_str(),
                s.is_definition,
                s.is_virtual,
                s.signature.as_str(),
            )
        })
        .collect();
    assert_eq!(
        on_start,
        [
            ("Acme::Net::IListener", false, true, "(Request)"),
            ("Acme::Net::Channel", true, true, "(Request)")
        ]
    );
    assert!(sym(&f, "ToString", SymbolKind::Method).is_virtual);
    assert!(!sym(&f, "Dispose", SymbolKind::Method).is_virtual);
    assert_eq!(
        call(&f, "Helper").container.as_deref(),
        Some("Acme::Net::Channel::OnStart")
    );
    assert_eq!(call(&f, "Run").line, 14);
    assert_eq!(
        call(&f, "Write").container.as_deref(),
        Some("Acme::Net::Channel::Dispose")
    );
    assert_eq!(call(&f, "Response").line, 16);
    assert_eq!(f.parse_errors, 0);

    let scoped = "namespace A.B;\nclass C { void M() {} }\n";
    let f = CSharpExtractor.extract(scoped.as_bytes()).unwrap();
    assert_eq!(sym(&f, "M", SymbolKind::Method).scope, "A::B::C");
}
