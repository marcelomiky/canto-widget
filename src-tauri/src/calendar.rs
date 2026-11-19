use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::calendar_event::{EventList, RawEvent};
use crate::error::{AppError, Result};

const CALENDAR_LIST_API: &str = "https://www.googleapis.com/calendar/v3/users/me/calendarList";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AgendaItem {
    pub id: String,
    pub title: String,
    /// RFC3339 when there's a time; YYYY-MM-DD for an all-day event.
    pub start: String,
    pub end: String,
    pub all_day: bool,
    pub location: String,
    pub meet: String,
    pub link: String,
    // Defaults keep task reminders and snoozed alerts, built without these, deserializable.
    #[serde(default)]
    pub organizer: String,
    #[serde(default)]
    pub creator: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub guests: u32,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// The user's own RSVP (accepted, declined, tentative, needsAction); empty when not on the guest list.
    #[serde(default)]
    pub response: String,
    #[serde(default)]
    pub attendees: Vec<Guest>,
    /// Short state for alerts that aren't calendar events: a Status API indicator or a model's rank.
    #[serde(default)]
    pub tag: String,
    /// Who triggered the alert (a mention on GitHub or GitLab); `None` when the forge did not say.
    #[serde(default)]
    pub sender: Option<Sender>,
}

/// The person behind a mention alert; `avatar` is a `data:` URL because the CSP allows no remote image.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Sender {
    pub name: String,
    pub login: String,
    pub avatar: String,
}

/// A person on the guest list; rooms are left out.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Guest {
    pub name: String,
    pub email: String,
    pub response: String,
    pub organizer: bool,
    pub optional: bool,
    pub me: bool,
}

/// A file linked to the event, such as the notes Gemini writes after a Meet.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Attachment {
    pub title: String,
    pub url: String,
    pub mime: String,
}

#[derive(Deserialize)]
struct CalendarList {
    #[serde(default)]
    items: Vec<CalendarEntry>,
    #[serde(default, rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct CalendarEntry {
    #[serde(default)]
    id: String,
}

/// Largest page Google Calendar serves.
const PAGE_MAX: u32 = 250;

/// Returns events from all calendars available to the authenticated account.
pub fn events(token: &str, time_min: &str, time_max: &str, max_results: u32) -> Result<Vec<AgendaItem>> {
    events_from_all_calendars(token, time_min, time_max, max_results as usize)
}

/// Returns events from all calendars, following pagination until the global cap.
pub fn events_all(token: &str, time_min: &str, time_max: &str, cap: usize) -> Result<Vec<AgendaItem>> {
    events_from_all_calendars(token, time_min, time_max, cap)
}

fn events_from_all_calendars(token: &str, time_min: &str, time_max: &str, cap: usize) -> Result<Vec<AgendaItem>> {
    if cap == 0 {
        return Ok(Vec::new());
    }

    let calendars = calendar_ids(token)?;
    let mut out = Vec::new();

    // Fetch up to `cap` events from each calendar before applying the
    // global limit. This avoids omitting earlier events from later calendars.
    for calendar_id in calendars {
        let mut items = collect_pages(cap, |next| page(token, &calendar_id, time_min, time_max, PAGE_MAX, next))?;

        out.append(&mut items);
    }

    // Each calendar is individually ordered by start time. Merge the results.
    Ok(sort_and_limit_events(out, cap))
}

fn sort_and_limit_events(mut events: Vec<AgendaItem>, cap: usize) -> Vec<AgendaItem> {
    events.sort_by_key(|event| event_start_order(&event.start));
    events.truncate(cap);
    events
}

/// Converts RFC3339 timestamps and all-day dates into comparable UTC timestamps.
/// All-day dates have no timezone, so midnight UTC is used as a deterministic reference.
fn event_start_order(start: &str) -> (u8, i64) {
    let timestamp =
        chrono::DateTime::parse_from_rfc3339(start).map(|datetime| datetime.timestamp_millis()).or_else(|_| {
            chrono::NaiveDate::parse_from_str(start, "%Y-%m-%d")
                .map(|date| date.and_hms_opt(0, 0, 0).expect("midnight is a valid time").and_utc().timestamp_millis())
        });

    match timestamp {
        Ok(timestamp) => (0, timestamp),
        Err(_) => (1, 0),
    }
}

/// Lists all calendar IDs visible to the authenticated Google account.
fn calendar_ids(token: &str) -> Result<Vec<String>> {
    let mut ids = collect_pages(usize::MAX, |next| calendar_list_page(token, next))?;

    // Avoid querying the same calendar twice if Google returns duplicates.
    let mut seen = HashSet::new();
    ids.retain(|id| !id.is_empty() && seen.insert(id.clone()));

    Ok(ids)
}

fn calendar_list_page(token: &str, page_token: Option<&str>) -> Result<(Vec<String>, Option<String>)> {
    let mut query = vec![("maxResults", PAGE_MAX.to_string()), ("minAccessRole", "reader".to_string())];

    if let Some(t) = page_token {
        query.push(("pageToken", t.to_string()));
    }

    let res = crate::google::api::client()?
        .get(CALENDAR_LIST_API)
        .bearer_auth(token)
        .query(&query)
        .send()
        .map_err(|e| AppError::Drive(crate::error::net_text(e)))?;

    if !res.status().is_success() {
        let status = res.status();
        return Err(AppError::Drive(format!("calendarList respondeu {status}: {}", res.text().unwrap_or_default())));
    }

    let list: CalendarList = res.json().map_err(|e| AppError::Drive(crate::error::net_text(e)))?;

    let ids = list.items.into_iter().map(|item| item.id).collect();

    Ok((ids, list.next_page_token))
}

pub(crate) fn collect_pages<T>(
    cap: usize,
    mut fetch: impl FnMut(Option<&str>) -> Result<(Vec<T>, Option<String>)>,
) -> Result<Vec<T>> {
    let mut out = Vec::new();
    let mut next: Option<String> = None;

    loop {
        let (items, token) = fetch(next.as_deref())?;
        out.extend(items);

        match token.filter(|t| !t.is_empty()) {
            Some(t) if out.len() < cap => next = Some(t),
            _ => break,
        }
    }

    out.truncate(cap);
    Ok(out)
}

fn events_url(calendar_id: &str) -> Result<url::Url> {
    let mut url = url::Url::parse("https://www.googleapis.com/calendar/v3/calendars")
        .map_err(|e| AppError::Drive(e.to_string()))?;

    url.path_segments_mut()
        .map_err(|_| AppError::Drive("URL de calendário inválida".into()))?
        .push(calendar_id)
        .push("events");

    Ok(url)
}

fn page(
    token: &str,
    calendar_id: &str,
    time_min: &str,
    time_max: &str,
    max_results: u32,
    page_token: Option<&str>,
) -> Result<(Vec<AgendaItem>, Option<String>)> {
    let mut query = vec![
        ("timeMin", time_min.to_string()),
        ("timeMax", time_max.to_string()),
        ("singleEvents", "true".to_string()),
        ("orderBy", "startTime".to_string()),
        ("maxResults", max_results.to_string()),
    ];

    if let Some(t) = page_token {
        query.push(("pageToken", t.to_string()));
    }

    let url = events_url(calendar_id)?;

    let res = crate::google::api::client()?
        .get(url)
        .bearer_auth(token)
        .query(&query)
        .send()
        .map_err(|e| AppError::Drive(crate::error::net_text(e)))?;

    if !res.status().is_success() {
        let status = res.status();
        return Err(AppError::Drive(format!("calendar respondeu {status}: {}", res.text().unwrap_or_default())));
    }

    let list: EventList = res.json().map_err(|e| AppError::Drive(crate::error::net_text(e)))?;

    Ok((list.items.into_iter().filter_map(RawEvent::into_item).collect(), list.next_page_token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_are_followed_until_the_last_one() {
        let mut asked = Vec::new();

        let out = collect_pages(100, |next| {
            asked.push(next.map(String::from));

            Ok(match next {
                None => (vec![1, 2], Some("p2".to_string())),
                _ => (vec![3], None),
            })
        })
        .unwrap();

        assert_eq!(out, vec![1, 2, 3]);
        assert_eq!(asked, vec![None, Some("p2".to_string())]);
    }

    #[test]
    fn a_busy_calendar_stops_at_the_cap_instead_of_paging_forever() {
        let mut calls = 0;

        let out = collect_pages(5, |_| {
            calls += 1;
            Ok((vec![0; 3], Some("again".to_string())))
        })
        .unwrap();

        assert_eq!((out.len(), calls), (5, 2));
    }

    #[test]
    fn a_failed_page_fails_the_whole_list_rather_than_returning_a_silent_part() {
        let r = collect_pages(100, |next| match next {
            None => Ok((vec![1], Some("p2".to_string()))),
            _ => Err(AppError::Drive("calendar respondeu 500".into())),
        });

        assert!(r.is_err());
    }

    fn event(start: &str) -> AgendaItem {
        AgendaItem { start: start.to_string(), ..Default::default() }
    }

    #[test]
    fn events_url_uses_calendar_id_as_single_path_segment() {
        let url = events_url("team@group.calendar.google.com").unwrap();
        let segments: Vec<_> = url.path_segments().unwrap().collect();

        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("www.googleapis.com"));
        assert_eq!(segments.len(), 5);
        assert_eq!(segments[0], "calendar");
        assert_eq!(segments[1], "v3");
        assert_eq!(segments[2], "calendars");
        assert!(segments[3].contains("group.calendar.google.com"));
        assert_eq!(segments[4], "events");
    }

    #[test]
    fn events_are_sorted_by_absolute_time_across_timezones() {
        let events = sort_and_limit_events(
            vec![
                event("2026-11-19T10:00:00-03:00"), // 13:00 UTC
                event("2026-11-19T08:30:00-04:00"), // 12:30 UTC
                event("2026-11-19T09:00:00-03:00"), // 12:00 UTC
            ],
            3,
        );

        let starts: Vec<_> = events.iter().map(|event| event.start.as_str()).collect();

        assert_eq!(
            starts,
            vec!["2026-11-19T09:00:00-03:00", "2026-11-19T08:30:00-04:00", "2026-11-19T10:00:00-03:00",]
        );
    }

    #[test]
    fn all_day_events_are_sorted_using_their_calendar_date() {
        let events = sort_and_limit_events(vec![event("2026-11-19T00:30:00Z"), event("2026-11-19")], 2);

        assert_eq!(events[0].start, "2026-11-19");
        assert_eq!(events[1].start, "2026-11-19T00:30:00Z");
    }

    #[test]
    fn sorted_events_respect_the_global_cap() {
        let events = sort_and_limit_events(
            vec![event("2026-11-19T12:00:00Z"), event("2026-11-19T09:00:00Z"), event("2026-11-19T10:00:00Z")],
            2,
        );

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].start, "2026-11-19T09:00:00Z");
        assert_eq!(events[1].start, "2026-11-19T10:00:00Z");
    }
}
