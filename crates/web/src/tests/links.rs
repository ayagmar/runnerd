//! Insights and Capacity link to each other's section for one profile by its name (`views::profile_id`): a link
//! lands on that profile's section whatever order or set of profiles the other page holds, and only an insight whose
//! kind names a profile links to one.

use runnerd_core::api::{InsightKind, Insights, Sizing};

use super::{Dashboard, fake};
use crate::views::profile_id;

/// The dashboard of a fake daemon answering `/v1/insights` and `/v1/sizing` with these, signed in.
async fn dashboard(insights: Insights, sizing: Sizing) -> (Dashboard, String) {
    let mut fixtures = fake::Fixtures::new(runnerd_fixtures::NOW);
    fixtures.insights = Some(insights);
    fixtures.sizing = Some(sizing);
    let dashboard = Dashboard::serving(&[], fixtures).await;
    let cookie = dashboard.sign_in().await;
    (dashboard, cookie)
}

/// Every fragment a page links to on `target`, such as `/sizing`, in order.
fn fragments(page: &str, target: &str) -> Vec<String> {
    let start = format!(r#"href="{target}#"#);
    page.match_indices(&start)
        .map(|(at, _)| {
            let rest = &page[at + start.len()..];
            rest[..rest.find('"').unwrap()].to_owned()
        })
        .collect()
}

/// The markup from the element with `id` to the end of its heading, or `None` without one.
fn headed(page: &str, id: &str) -> Option<String> {
    let at = page.find(&format!(r#"id="{id}""#))?;
    let rest = &page[at..];
    let end = ["</h2>", "</h3>"]
        .iter()
        .filter_map(|close| rest.find(close))
        .min()?;
    Some(rest[..end].to_owned())
}

/// The fixtures with each profile renamed by `rename`, in their own field wherever they name one.
fn renamed(rename: impl Fn(&str) -> String) -> (Insights, Sizing) {
    let mut insights = runnerd_fixtures::insights(runnerd_fixtures::NOW);
    for insight in &mut insights.insights {
        if let InsightKind::NearCap { profile, .. }
        | InsightKind::CpuHeld { profile, .. }
        | InsightKind::StoreOverflow { profile, .. } = &mut insight.kind
        {
            *profile = rename(profile);
        }
    }
    for queue in &mut insights.queue {
        queue.profile = rename(&queue.profile);
    }
    let mut sizing = runnerd_fixtures::sizing(runnerd_fixtures::NOW);
    sizing.auto = false;
    for profile in &mut sizing.profiles {
        profile.name = rename(&profile.name);
    }
    for change in &mut sizing.changes {
        change.profile = rename(&change.profile);
    }
    (insights, sizing)
}

/// Catches a positional anchor (`#sizing-0`) followed from Insights: with Capacity's profiles in another order, it
/// opened another profile's section.
#[tokio::test]
async fn a_profile_link_lands_on_that_profile_whatever_order_the_other_page_lists_them_in() {
    let (insights, mut sizing) = renamed(|name| format!("{name}-2.x"));
    sizing.profiles.reverse();
    let (dashboard, cookie) = dashboard(insights.clone(), sizing.clone()).await;
    let insights_page = dashboard.page(&cookie, "/insights").await.body;
    let sizing_page = dashboard.page(&cookie, "/sizing").await.body;

    let to_sizing = fragments(&insights_page, "/sizing");
    assert!(
        !to_sizing.is_empty(),
        "Insights links profiles to Capacity: {insights_page}"
    );
    let mut landed = 0;
    for fragment in &to_sizing {
        let name = insights
            .queue
            .iter()
            .map(|queue| queue.profile.as_str())
            .chain(
                insights
                    .insights
                    .iter()
                    .filter_map(|insight| match &insight.kind {
                        InsightKind::NearCap { profile, .. }
                        | InsightKind::CpuHeld { profile, .. }
                        | InsightKind::StoreOverflow { profile, .. } => Some(profile.as_str()),
                        _ => None,
                    }),
            )
            .find(|name| profile_id(name) == *fragment)
            .unwrap_or_else(|| panic!("#{fragment} is no profile Insights shows"));
        if let Some(section) = headed(&sizing_page, fragment) {
            assert!(
                section.contains(name),
                "#{fragment} opens {name}'s section: {section}"
            );
            landed += 1;
        }
    }
    assert!(
        landed >= 2,
        "the profiles both pages hold are found on Capacity"
    );

    let to_insights = fragments(&sizing_page, "/insights");
    assert_eq!(
        to_insights.len(),
        sizing
            .profiles
            .iter()
            .filter(|profile| profile.starts.total() > 0)
            .count(),
        "each profile with jobs links to why they queued: {sizing_page}"
    );
    for fragment in &to_insights {
        let profile = sizing
            .profiles
            .iter()
            .find(|profile| profile_id(&profile.name) == *fragment)
            .unwrap();
        if let Some(section) = headed(&insights_page, fragment) {
            assert!(
                section.contains(&profile.name),
                "#{fragment} opens {}'s queue: {section}",
                profile.name
            );
        }
    }
    // Capacity's own lanes reach its sections by the same ids.
    for fragment in fragments(&sizing_page, "") {
        assert!(
            headed(&sizing_page, &fragment).is_some(),
            "#{fragment} is a section on Capacity"
        );
    }
}

/// Catches a profile Capacity no longer lists linked from Insights to another profile's section, as a positional or
/// a lossy anchor would.
#[tokio::test]
async fn a_profile_capacity_lacks_links_to_no_other_profile() {
    let (insights, mut sizing) = renamed(str::to_owned);
    let gone = insights.queue[0].profile.clone();
    sizing.profiles.retain(|profile| profile.name != gone);
    sizing.changes.retain(|change| change.profile != gone);
    let (dashboard, cookie) = dashboard(insights, sizing.clone()).await;
    let insights_page = dashboard.page(&cookie, "/insights").await.body;
    let sizing_page = dashboard.page(&cookie, "/sizing").await.body;

    let fragment = profile_id(&gone);
    assert!(
        fragments(&insights_page, "/sizing").contains(&fragment),
        "Insights still links {gone} by its own id"
    );
    assert!(
        headed(&sizing_page, &fragment).is_none(),
        "Capacity has no section for {gone}, so the link opens its top"
    );
    for profile in &sizing.profiles {
        assert_ne!(
            profile_id(&profile.name),
            fragment,
            "{gone} is not {}",
            profile.name
        );
    }
}

/// Catches a host's or a repository's insight given some profile's link, such as the first profile's.
#[tokio::test]
async fn an_insight_about_the_host_or_a_repository_links_to_no_profile() {
    let (insights, sizing) = renamed(str::to_owned);
    let (dashboard, cookie) = dashboard(insights.clone(), sizing).await;
    let page = dashboard.page(&cookie, "/insights").await.body;
    let list = &page[page.find(r#"<ul class="insight-list""#).unwrap()..];
    let list = &list[..list.find("</ul>").unwrap()];
    let cards: Vec<&str> = list.split(r#"<li class="insight "#).skip(1).collect();
    assert_eq!(cards.len(), insights.insights.len());
    let mut without = 0;
    for (card, insight) in cards.iter().zip(&insights.insights) {
        let links = fragments(card, "/sizing");
        match &insight.kind {
            InsightKind::NearCap { profile, .. }
            | InsightKind::CpuHeld { profile, .. }
            | InsightKind::StoreOverflow { profile, .. } => {
                assert_eq!(links, [profile_id(profile)], "{}", insight.key);
            }
            _ => {
                assert!(
                    links.is_empty(),
                    "{} links to no profile: {card}",
                    insight.key
                );
                without += 1;
            }
        }
    }
    assert!(
        without >= 2,
        "the fixtures hold a host's and a repository's insight"
    );
}
