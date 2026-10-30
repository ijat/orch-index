use oi_lang::{CppExtractor, Extractor};
use oi_model::{FileFacts, RefKind, SymbolKind};

const SRC: &str = r#"#include "nsHttpChannel.h"
#include <vector>
#define MAX_SIZE 10

namespace mozilla {
namespace net {

class nsHttpChannel final : public HttpBaseChannel, public nsIStreamListener {
 public:
  NS_IMETHOD OnStartRequest(nsIRequest* aRequest) override;
  virtual void Cancel(nsresult aStatus, const nsACString& aReason);
  void Plain(int);
  int mCount;
};

NS_IMETHODIMP nsHttpChannel::OnStartRequest(nsIRequest* request) {
  nsCOMPtr<nsIURI> uri(request);
  int local = Helper(request);
  mListener->OnStartRequest(this);
  return NS_OK;
}

static int Helper(nsIRequest* r) { return 0; }

}  // namespace net
}  // namespace mozilla
"#;

fn facts() -> FileFacts {
    CppExtractor::default().extract(SRC.as_bytes()).unwrap()
}

fn find<'a>(f: &'a FileFacts, name: &str, kind: SymbolKind) -> Vec<&'a oi_model::Symbol> {
    f.symbols
        .iter()
        .filter(|s| s.name == name && s.kind == kind)
        .collect()
}

#[test]
fn includes() {
    let f = facts();
    let specs: Vec<_> = f
        .includes
        .iter()
        .map(|i| (i.spec.as_str(), i.system, i.line))
        .collect();
    assert_eq!(specs, [("nsHttpChannel.h", false, 1), ("vector", true, 2)]);
}

#[test]
fn class_and_bases() {
    let f = facts();
    let c = find(&f, "nsHttpChannel", SymbolKind::Class);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].scope, "mozilla::net");
    assert_eq!(c[0].line, 8);
    let bases: Vec<_> = f.bases.iter().map(|b| b.base.as_str()).collect();
    assert_eq!(bases, ["HttpBaseChannel", "nsIStreamListener"]);
}

#[test]
fn methods_decl_and_out_of_line_def() {
    let f = facts();
    let m = find(&f, "OnStartRequest", SymbolKind::Method);
    assert_eq!(m.len(), 2, "{m:#?}");
    for s in &m {
        assert_eq!(s.scope, "mozilla::net::nsHttpChannel");
        assert_eq!(s.signature, "(nsIRequest*)");
        assert!(s.is_virtual || s.is_definition);
    }
    assert!(m.iter().any(|s| s.is_definition && s.line == 16));
    assert!(
        m.iter()
            .any(|s| !s.is_definition && s.is_virtual && s.line == 10)
    );
    let cancel = find(&f, "Cancel", SymbolKind::Method);
    assert_eq!(cancel[0].signature, "(nsresult, const nsACString&)");
    assert!(cancel[0].is_virtual);
    assert!(!find(&f, "Plain", SymbolKind::Method)[0].is_virtual);
    assert_eq!(find(&f, "mCount", SymbolKind::Field).len(), 1);
    assert_eq!(find(&f, "MAX_SIZE", SymbolKind::Macro).len(), 1);
    assert_eq!(
        find(&f, "Helper", SymbolKind::Function)[0].scope,
        "mozilla::net"
    );
}

#[test]
fn locals_are_not_symbols() {
    let f = facts();
    assert!(
        f.symbols
            .iter()
            .all(|s| s.name != "uri" && s.name != "local"),
        "{:#?}",
        f.symbols
    );
}

#[test]
fn references() {
    let f = facts();
    let has = |name: &str, kind, line| {
        f.references
            .iter()
            .any(|r| r.name == name && r.kind == kind && r.line == line)
    };
    assert!(has("Helper", RefKind::Call, 18));
    let call = f.references.iter().find(|r| r.name == "Helper").unwrap();
    assert_eq!(
        call.container.as_deref(),
        Some("mozilla::net::nsHttpChannel::OnStartRequest")
    );
    assert!(has("OnStartRequest", RefKind::Call, 19));
    assert!(has("nsIRequest", RefKind::Type, 10));
    // The definition name itself is not a reference.
    assert!(
        !f.references
            .iter()
            .any(|r| r.name == "OnStartRequest" && r.line == 16)
    );
}
