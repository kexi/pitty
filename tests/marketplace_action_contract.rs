//! Contract gate: `action.yml` must stay publishable to the GitHub Marketplace.
//!
//! Publishing an Action to the Marketplace has fixed metadata requirements that
//! GitHub enforces in its (manual) publish UI. Those requirements are easy to
//! break by editing `action.yml` — e.g. dropping `branding`, or setting a color
//! GitHub does not allow — and the breakage is only discovered when someone
//! tries to publish a release. This test pins the requirements so a regression
//! fails at `cargo test` time instead, in the same spirit as
//! `release_asset_name_contract.rs` and `schema_contract.rs`.
//!
//! Requirements checked (per the GitHub Marketplace publishing rules):
//! - `action.yml` lives at the repository root (this file reaches it via
//!   `../action.yml`, so a move breaks compilation — the strongest possible gate).
//! - `name:` is present (and is the unique Marketplace listing name).
//! - `description:` is present.
//! - `branding:` is present with both `icon:` and `color:`.
//! - `color:` is one of the eight colors GitHub allows.
//! - `icon:` is a single Feather icon name (Marketplace rejects unknown icons);
//!   we cannot vendor the whole Feather set, so we assert the value is a plain
//!   lowercase-kebab token and is not one of the icons GitHub explicitly blocks.
//!
//! What this does NOT do: it cannot press the "Publish this Action to the GitHub
//! Marketplace" button — that first publish requires accepting the Marketplace
//! agreement in the web UI and has no workflow/API switch. The first publish is
//! already done for this repository, so later published GitHub Releases are the
//! Marketplace update path. This gate guarantees those releases cannot fail on a
//! metadata regression.

/// The composite action source, embedded so the test reads exactly what ships.
const ACTION_YML: &str = include_str!("../action.yml");

/// The eight colors GitHub allows for `branding.color`.
const ALLOWED_COLORS: &[&str] = &[
    "white",
    "yellow",
    "blue",
    "green",
    "orange",
    "red",
    "purple",
    "gray-dark",
];

/// Icons GitHub explicitly disallows for `branding.icon` (its own brand marks
/// and a few reserved logos). A full Feather-set check is infeasible to vendor,
/// so we gate on the documented blocklist plus a shape check below.
const BLOCKED_ICONS: &[&str] = &[
    "coffee",
    "columns",
    "divide",
    "rotate-cw",
    "rotate-ccw",
    "code",
    "key",
    "trash",
    "trash-2",
    "github",
    "gitlab",
    "x",
    "slack",
    "twitter",
    "facebook",
    "instagram",
    "linkedin",
    "youtube",
];

/// Read the scalar value of a top-level `key:` line from `action.yml`.
///
/// Minimal on purpose: `action.yml`'s top-level metadata keys are simple
/// `key: value` scalars, so a full YAML parser is unnecessary (and would add a
/// dependency the project deliberately avoids). Returns the trimmed value.
fn top_level_scalar(key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    ACTION_YML
        .lines()
        .find(|line| line.starts_with(&prefix))
        .map(|line| line[prefix.len()..].trim().to_string())
}

/// Read the value of a key nested one level (two-space indent) under `branding:`.
fn branding_scalar(key: &str) -> Option<String> {
    let mut in_branding = false;
    for line in ACTION_YML.lines() {
        if line.starts_with("branding:") {
            in_branding = true;
            continue;
        }
        if in_branding {
            // A new top-level key (no indent, non-empty) ends the branding block.
            if !line.is_empty() && !line.starts_with(' ') {
                break;
            }
            let needle = format!("  {key}:");
            if let Some(rest) = line.strip_prefix(&needle) {
                return Some(rest.trim().to_string());
            }
        }
    }
    None
}

#[test]
fn action_has_name_and_description() {
    let name = top_level_scalar("name").expect("action.yml must declare a top-level `name:`");
    assert!(
        !name.is_empty(),
        "Marketplace listing requires a non-empty `name:`"
    );
    let description =
        top_level_scalar("description").expect("action.yml must declare a `description:`");
    assert!(
        !description.is_empty(),
        "Marketplace listing requires a non-empty `description:`"
    );
}

#[test]
fn action_has_branding_block() {
    assert!(
        ACTION_YML.lines().any(|l| l.starts_with("branding:")),
        "Marketplace publishing requires a top-level `branding:` block"
    );
    assert!(
        branding_scalar("icon").is_some(),
        "Marketplace `branding:` requires an `icon:`"
    );
    assert!(
        branding_scalar("color").is_some(),
        "Marketplace `branding:` requires a `color:`"
    );
}

#[test]
fn branding_color_is_allowed() {
    let color = branding_scalar("color").expect("branding.color must be present");
    assert!(
        ALLOWED_COLORS.contains(&color.as_str()),
        "branding.color {color:?} is not a GitHub-allowed color; must be one of {ALLOWED_COLORS:?}"
    );
}

#[test]
fn branding_icon_is_shaped_like_a_feather_icon() {
    let icon = branding_scalar("icon").expect("branding.icon must be present");
    // A Feather icon name is a single lowercase-kebab token (letters/digits/`-`).
    assert!(
        !icon.is_empty()
            && icon
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "branding.icon {icon:?} must be a single lowercase-kebab Feather icon name"
    );
    assert!(
        !BLOCKED_ICONS.contains(&icon.as_str()),
        "branding.icon {icon:?} is on GitHub's disallowed-icon list; pick another Feather icon"
    );
}
