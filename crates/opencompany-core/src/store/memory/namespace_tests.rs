use super::*;

fn id(raw: &str) -> CompanyId {
    CompanyId::new(raw)
}

#[test]
fn sanitized_collisions_stay_distinct_namespaces() {
    // The whole point: these three sanitize to the same prefix.
    let a = Namespace::company_root(&id("acme:1"));
    let b = Namespace::company_root(&id("acme/1"));
    let c = Namespace::company_root(&id("acme_1"));
    assert_ne!(a, b);
    assert_ne!(b, c);
    assert_ne!(a, c);
}

#[test]
fn derivation_is_stable_across_calls() {
    // Durability rests on this: a company's namespace must not move.
    assert_eq!(
        Namespace::company_root(&id("acme")),
        Namespace::company_root(&id("acme"))
    );
}

#[test]
fn an_empty_id_still_yields_a_namespace() {
    let ns = Namespace::company_root(&id(""));
    assert!(ns.as_str().starts_with("oc/h-"), "{}", ns.as_str());
}

#[test]
fn tenant_prefixed_ids_stay_distinct() {
    // Shared-single-DB mode prefixes ids with `<tenant>--`. Two tenants
    // booting the same company template must not share a namespace.
    let a = Namespace::company_root(&id("acme--software_company"));
    let b = Namespace::company_root(&id("globex--software_company"));
    assert_ne!(a, b);
}

#[test]
fn children_of_different_companies_never_collide() {
    let a = Namespace::company_root(&id("acme")).child(&Scope::Facts);
    let b = Namespace::company_root(&id("globex")).child(&Scope::Facts);
    assert_ne!(a, b);
}

#[test]
fn scopes_partition_one_company() {
    let root = Namespace::company_root(&id("acme"));
    let scopes = [
        Scope::Facts,
        Scope::Traces,
        Scope::Archive,
        Scope::TaskResults,
        Scope::Context,
        Scope::Scratch,
        Scope::Agent("cto".into()),
        Scope::Desk("eng".into()),
    ];
    let mut seen = std::collections::HashSet::new();
    for scope in &scopes {
        assert!(
            seen.insert(root.child(scope)),
            "two scopes collided: {scope:?}"
        );
    }
}

#[test]
fn agent_ids_that_sanitize_alike_stay_distinct() {
    // `a:b` and `a/b` both sanitize to `a_b`. Merging them would point two
    // agents at one namespace, and each could read the other's private
    // partition.
    let root = Namespace::company_root(&id("acme"));
    let a = root.child(&Scope::Agent("a:b".into()));
    let b = root.child(&Scope::Agent("a/b".into()));
    let c = root.child(&Scope::Agent("a_b".into()));
    assert_ne!(a, b);
    assert_ne!(b, c);
    assert_ne!(a, c);
}

#[test]
fn desk_ids_that_sanitize_alike_stay_distinct() {
    let root = Namespace::company_root(&id("acme"));
    assert_ne!(
        root.child(&Scope::Desk("a:b".into())),
        root.child(&Scope::Desk("a/b".into()))
    );
}

#[test]
fn an_agent_and_a_desk_with_the_same_id_stay_apart() {
    // They differ by the `agent/` vs `desk/` prefix, not by the member, so
    // this holds independently of how the member is derived.
    let root = Namespace::company_root(&id("acme"));
    assert_ne!(
        root.child(&Scope::Agent("ops".into())),
        root.child(&Scope::Desk("ops".into()))
    );
}

#[test]
fn a_member_id_that_sanitizes_to_nothing_still_names_a_member() {
    // Otherwise `child` builds a namespace ending in `/`, naming the scope
    // kind rather than any member of it.
    let root = Namespace::company_root(&id("acme"));
    let empty = root.child(&Scope::Agent(String::new()));
    assert!(!empty.as_str().ends_with('/'), "{}", empty.as_str());
    assert!(empty.as_str().contains("agent/h-"), "{}", empty.as_str());
    // And still distinct from a different unsanitizable id.
    assert_ne!(empty, root.child(&Scope::Agent(":".into())));
}

#[test]
fn member_derivation_is_stable_across_calls() {
    let root = Namespace::company_root(&id("acme"));
    assert_eq!(
        root.child(&Scope::Agent("cto".into())),
        root.child(&Scope::Agent("cto".into()))
    );
}

/// Whether the engine's `folder` filter for `wanted` admits an item filed
/// under `held` — the `/`-aware prefix rule `migrate` and every scope rely on.
fn folder_admits(wanted: &str, held: &str) -> bool {
    use tinymemory::{ItemKind, MemoryMeta, MetaFilter};
    let filter = MetaFilter {
        folder: Some(wanted.to_string()),
        ..MetaFilter::default()
    };
    let meta = MemoryMeta {
        folder: Some(held.to_string()),
        ..MemoryMeta::default()
    };
    filter.matches(ItemKind::Document, &meta)
}

#[test]
fn the_folder_filter_is_boundary_aware() {
    // `oc/acme-1` must not swallow `oc/acme-10` — a prefix test without the
    // separator check would hand one company another's entries.
    assert!(folder_admits("oc/acme-1", "oc/acme-1"));
    assert!(folder_admits("oc/acme-1", "oc/acme-1/facts"));
    assert!(!folder_admits("oc/acme-1", "oc/acme-10"));
    assert!(!folder_admits("oc/acme-1", "oc/acme-10/facts"));
    assert!(!folder_admits("oc/acme-1", "oc/globex-2/facts"));
}

#[test]
fn a_company_root_never_admits_another_companys_namespace() {
    let a = Namespace::company_root(&id("acme"));
    let b = Namespace::company_root(&id("globex"));
    assert!(!folder_admits(a.as_str(), b.as_str()));
    assert!(!folder_admits(b.as_str(), a.as_str()));
}

#[test]
fn every_namespace_is_under_the_host_root() {
    let root = Namespace::company_root(&id("acme"));
    let agent = root.child(&Scope::Agent("cto".into()));
    assert!(folder_admits(root.as_str(), agent.as_str()));
    assert!(folder_admits(ROOT, agent.as_str()));
}
