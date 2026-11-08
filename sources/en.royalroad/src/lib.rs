#![no_std]
use buny::{
	Chapter, ContentBlock, ContentRating, FilterValue, Novel, NovelPageResult, NovelStatus, Result,
	Source,
	alloc::{String, Vec, string::ToString, vec},
	helpers::{element::ElementHelpers, uri::QueryParameters},
	imports::{defaults::defaults_get, html::Document, net::Request},
	prelude::*,
};
use chapter_numbers::{chapter_numbers, chapter_title};
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub mod traits;

// to create a source, you need a struct that implements the Source trait
// the struct can contain properties that are initialized with the new() method
struct RoyalRoad;

const BASE_URL: &str = "https://royalroad.com";

const ANUBIS_MARKER: &str = "<script id=\"anubis_challenge\" type=\"application/json\">";

#[derive(Deserialize)]
struct AnubisPage {
	rules: AnubisRules,
	challenge: AnubisChallenge,
}

#[derive(Deserialize)]
struct AnubisRules {
	algorithm: String,
	difficulty: usize,
}

#[derive(Deserialize)]
struct AnubisChallenge {
	id: String,
	#[serde(rename = "randomData")]
	random_data: String,
}

/// Loads a Royal Road page, getting past Anubis first if it asks.
///
/// Since 2026-10 the fiction lists and pages answer with an Anubis
/// proof-of-work page (v1.28.3, algorithm "fast", difficulty 4) until the
/// `techaro.lol-anubis-auth-*` cookie is set. The app keeps that cookie, so
/// this only runs about once a week or after the IP changes. `/home` is not
/// checked.
pub(crate) fn fetch_html(url: &str) -> Result<Document> {
	let response = Request::get(url)?.send()?;
	let text = response.get_string()?;
	let Some(start) = text.find(ANUBIS_MARKER) else {
		return Ok(response.get_html()?);
	};
	let json = &text[start + ANUBIS_MARKER.len()..];
	let json = &json[..json.find("</script>").unwrap_or(json.len())];
	let page: AnubisPage =
		serde_json::from_str(json).map_err(|_| error!("Unreadable Anubis check on Royal Road"))?;
	// "slow" only differs in how the browser schedules the work.
	if page.rules.algorithm != "fast" && page.rules.algorithm != "slow" {
		return Err(error!(
			"Royal Road asks for an unsupported Anubis check ({})",
			page.rules.algorithm
		));
	}
	let (nonce, hash) = solve_anubis(&page.challenge.random_data, page.rules.difficulty);

	let mut qs = QueryParameters::new();
	qs.push("id", Some(&page.challenge.id));
	qs.push("response", Some(&hash));
	qs.push("nonce", Some(&nonce.to_string()));
	qs.push("redir", Some(url.strip_prefix(BASE_URL).unwrap_or("/")));
	// ponytail: Anubis only logs this, so a plausible constant does.
	qs.push("elapsedTime", Some("1000"));
	// royalroad.com redirects to www, so the challenge and its cookies are
	// there. This sets the auth cookie and redirects back to the page.
	let response = Request::get(format!(
		"https://www.royalroad.com/.within.website/x/cmd/anubis/api/pass-challenge?{qs}"
	))?
	.send()?;
	if response.get_string()?.contains(ANUBIS_MARKER) {
		return Err(error!("Royal Road's Anubis check failed. Try again later."));
	}
	Ok(response.get_html()?)
}

/// Finds the first nonce where SHA-256(`data` + nonce) starts with
/// `difficulty` zero hex digits, like Anubis' worker. Returns the nonce and
/// the hash as hex.
fn solve_anubis(data: &str, difficulty: usize) -> (u64, String) {
	(0u64..)
		.find_map(|nonce| {
			let hash = Sha256::digest(format!("{data}{nonce}"));
			hash.iter()
				.flat_map(|b| [b >> 4, b & 0xf])
				.take(difficulty)
				.all(|nibble| nibble == 0)
				.then(|| (nonce, hash.iter().map(|b| format!("{b:02x}")).collect()))
		})
		.unwrap()
}

impl Source for RoyalRoad {
	// this method is called once when the source is initialized
	// perform any necessary setup here
	fn new() -> Self {
		Self
	}

	// this method will be called first without a query when the search page is opened,
	// then when a search query is entered or filters are changed

	fn get_search_novel_list(
		&self,
		query: Option<String>,
		page: i32,
		filters: Vec<FilterValue>,
	) -> Result<NovelPageResult> {
		// https://novelfire.net/search?keyword=shadow&page=4
		let mut qs = QueryParameters::new();
		qs.push("globalFilters", Some("false"));
		qs.push("title", query.as_deref());
		qs.push("page", Some(&page.to_string()));

		for filter in filters {
			match filter {
				FilterValue::MultiSelect {
					included, excluded, ..
				} => {
					for id in included {
						qs.push("tagsAdd", Some(&id.to_lowercase()));
					}
					for id in excluded {
						qs.push("tagsRemove", Some(&id.to_lowercase()));
					}
				}
				FilterValue::Sort { id, .. } => {
					qs.push("orderBy", Some(&id));
				}
				FilterValue::Select { id, .. } => {
					qs.push("type", Some(&id));
				}
				FilterValue::Text { value, .. } => {
					qs.push("author", Some(&value));
				}
				_ => {}
			}
		}

		let url = format!("{}/fictions/search?{qs}", BASE_URL);
		let html = fetch_html(&url)?;
		let entries: Vec<Novel> = html
			.select(".fiction-list > .fiction-list-item")
			.map(|els| {
				els.filter_map(|novel_node| {
					let key = novel_node
						.select_first("a")?
						.attr("href")?
						.to_string()
						.replace("/fiction/", "");
					let title = String::from(
						novel_node
							.select_first("a:not(:has(img))")?
							.text()?
							.to_string()
							.trim(),
					);

					let mut cover = novel_node
						.select_first("a")?
						.select_first("img")?
						.attr("src")?
						.to_string();
					if !cover.starts_with("https://") {
						cover = format!("{}{cover}", BASE_URL);
					}

					let tags: Option<Vec<String>> =
						novel_node.select(".label:not(.pull-right)").map(|els| {
							els.filter_map(|el| {
								let tag = el.text().unwrap();
								Some(tag)
							})
							.collect()
						});

					let status = novel_node
						.select(".label")
						.map(|els| {
							els.filter_map(|el| {
								let statustxt = el.text().unwrap();
								match statustxt.as_str() {
									"COMPLETED" => Some(NovelStatus::Completed),
									"ONGOING" => Some(NovelStatus::Ongoing),
									"INACTIVE" => Some(NovelStatus::Hiatus),
									"HIATUS" => Some(NovelStatus::Hiatus),
									"CANCELLED" => Some(NovelStatus::Cancelled),
									_ => None,
								}
							})
							.next()
							.unwrap_or(NovelStatus::Unknown)
						})
						.unwrap_or(NovelStatus::Unknown);

					let description = novel_node
						.select(format!("#description-{}", key.split('/').next().unwrap()))
						.map(|els| {
							els.filter_map(|el| {
								let desc = el.untrimmed_text().unwrap();
								Some(desc)
							})
							.collect::<Vec<String>>()
							.join("\n\n")
						});

					Some(Novel {
						key,
						title,
						tags,
						status,
						description,
						cover: Some(cover),
						..Default::default()
					})
				})
				.collect()
			})
			.unwrap_or_default();

		let has_next_page = html
			.select_first(".pagination")
			.is_some_and(|el| !el.has_class("page-active"));

		Ok(NovelPageResult {
			entries,
			has_next_page,
		})
	}

	// this method will be called when a novel page is opened
	fn get_novel_update(
		&self,
		mut novel: Novel,
		needs_details: bool,
		needs_chapters: bool,
		_page: i32,
	) -> Result<Novel> {
		let url = format!("{}/fiction/{}", BASE_URL, novel.key);
		let html = fetch_html(&url)?;
		let info_div = html.select_first(".fiction-info").unwrap();

		if needs_details {
			let main_div = html.select_first(".fic-header img").unwrap();
			let mut cover = main_div.attr("src").unwrap();
			if !cover.starts_with("https://") {
				cover = format!("{}{cover}", BASE_URL);
			}
			let title = html.select_first(".fic-title h1").unwrap().text();
			let author = html.select_first(".fic-title a").unwrap().text().unwrap();

			let description = info_div.select(".description .hidden-content").map(|els| {
				els.filter_map(|el| {
					let desc = el.text_with_newlines().unwrap();
					Some(desc)
				})
				.collect::<Vec<String>>()
				.join("\n\n")
			});

			let content_rating = info_div
				.select(".text-center")
				.map(|els| {
					let rating = els.text().unwrap();
					if rating.contains("Sexual Content") {
						ContentRating::NSFW
					} else if rating.contains("Graphic Violence")
						|| rating.contains("Sensitive Content")
					{
						ContentRating::Suggestive
					} else {
						ContentRating::Safe
					}
				})
				.unwrap_or(ContentRating::Unknown);

			let tags: Option<Vec<String>> = info_div.select(".label:not(.pull-right)").map(|els| {
				els.filter_map(|el| {
					let tag = el.text().unwrap();
					Some(tag)
				})
				.collect()
			});

			let status = info_div
				.select(".label")
				.map(|els| {
					els.filter_map(|el| {
						let statustxt = el.text().unwrap();
						match statustxt.as_str() {
							"COMPLETED" => Some(NovelStatus::Completed),
							"ONGOING" => Some(NovelStatus::Ongoing),
							"INACTIVE" => Some(NovelStatus::Hiatus),
							"HIATUS" => Some(NovelStatus::Hiatus),
							"CANCELLED" => Some(NovelStatus::Cancelled),
							_ => None,
						}
					})
					.next()
					.unwrap_or(NovelStatus::Unknown)
				})
				.unwrap_or(NovelStatus::Unknown);

			if let Some(title) = title {
				novel.title = title;
			}

			novel.cover = Some(cover.to_string());
			novel.authors = Some(vec![author]);
			novel.description = description;
			novel.status = status;
			novel.content_rating = content_rating;
			novel.tags = tags;
			novel.url = Some(url);
		}
		if needs_chapters {
			let rows: Vec<(String, String, Option<i64>)> = info_div
				.select(".chapter-row")
				.map(|els| {
					els.filter_map(|el| {
						let link = el.select_first("a")?;
						let chapter_key = link
							.attr("href")?
							.replace(&format!("/fiction/{}/chapter/", novel.key), "");
						let name = link.text()?.trim().to_string();
						// `unixtime` is in seconds. `datetime` has seven fractional
						// digits, which the date parser does not take.
						let date_uploaded = el
							.select_first(".text-right a time")
							.and_then(|time| time.attr("unixtime"))
							.and_then(|seconds| seconds.trim().parse::<i64>().ok());
						Some((chapter_key, name, date_uploaded))
					})
					.collect()
				})
				.unwrap_or_default();
			let names: Vec<&str> = rows.iter().map(|(_, name, _)| name.as_str()).collect();
			let chapters: Vec<Chapter> = rows
				.iter()
				.zip(chapter_numbers(&names))
				.map(|((chapter_key, name, date_uploaded), number)| Chapter {
					key: chapter_key.clone(),
					chapter_number: Some(number),
					title: chapter_title(name, number),
					url: Some(format!(
						"{}/fiction/{}/chapter/{}",
						BASE_URL, novel.key, chapter_key
					)),
					date_uploaded: *date_uploaded,
					..Default::default()
				})
				.collect();

			novel.chapters = Some(chapters);
			novel.has_more_chapters = Some(false);
		}
		Ok(novel)
	}

	fn get_chapter_content_list(
		&self,
		novel: Novel,
		chapter: Chapter,
	) -> Result<Vec<ContentBlock>> {
		let url = format!("{}/fiction/{}/chapter/{}", BASE_URL, novel.key, chapter.key);
		let html = fetch_html(&url)?;

		let mut content_list: Vec<ContentBlock> = html
			.select(".chapter-content")
			.map(|els| {
				els.filter_map(|content_node| {
					// paragraph might have a "read at website" element in it so we use own_text.
					let content = content_node.text_with_newlines().unwrap();
					if content.starts_with('[') && content.ends_with(']') {
						let mut quote = content.chars();
						quote.next();
						quote.next_back();
						quote.as_str().to_string();

						return Some(ContentBlock::block_quote(quote.as_str().to_string()));
					} else if content == "***" {
						return Some(ContentBlock::Divider);
					}
					Some(ContentBlock::paragraph(content, None))
				})
				.collect()
			})
			.unwrap_or_default();

		// author note placement: "after" (default) | "before" | "hidden"
		let author_note_position =
			defaults_get::<String>("authorNotePosition").unwrap_or_else(|| "after".into());
		if author_note_position != "hidden" && html.select_first(".author-note").is_some() {
			let author_note = html
				.select(".author-note")
				.unwrap()
				.text_with_newlines()
				.unwrap();
			let author_note_title = html
				.select_first(".author-note-portlet .portlet-title")
				.unwrap()
				.text()
				.unwrap();
			let block = ContentBlock::BlockQuote(author_note_title + "\n" + &author_note);
			if author_note_position == "before" {
				content_list.insert(0, block);
			} else {
				content_list.push(block);
			}
		}

		let review_link = format!("LINK: [click here for chapter reviews.]({})", url);
		content_list.push(ContentBlock::Divider);
		content_list.push(ContentBlock::paragraph(review_link, None));
		Ok(content_list)
	}
}

// the register_source! macro generates the necessary wasm functions for buny
register_source!(
	RoyalRoad,
	// after the name of the source struct, list all the extra traits it implements
	ListingProvider,
	Home,
	DeepLinkHandler,
	ReviewProvider
);

#[cfg(test)]
mod test {
	use super::*;
	use buny::{Home, HomeComponentValue, Listing, ListingProvider, ReviewProvider};
	use buny_test::buny_test;

	#[buny_test]
	fn test_solve_anubis() {
		// Reference from the browser's crypto.subtle, same loop as Anubis' worker.
		let (nonce, hash) = solve_anubis(
			"2c5c8a309aa79a4d1f768e455392588d3243c4b2dd7a3f229d84e196f345adf5",
			4,
		);
		assert_eq!(nonce, 135045);
		assert_eq!(
			hash,
			"000037696c1da01a62bc7d40ce83506e98b42e6f59bbd01d0b8962ceb60774da"
		);
	}

	#[buny_test]
	fn test_chapter_numbers() {
		// "Prologue", then "Chapter 1 - The Void": the prologue goes before
		// chapter 1 instead of pushing every chapter up by one.
		let novel = RoyalRoad::new()
			.get_novel_update(
				Novel {
					key: "145896/dao-of-the-world-walker-craft-based-slice-of-life".into(),
					..Default::default()
				},
				false,
				true,
				1,
			)
			.unwrap();
		let chapters = novel.chapters.unwrap();
		println!("{} chapters", chapters.len());
		assert!(chapters.len() > 200);
		assert_eq!(chapters[0].chapter_number, Some(0.5));
		assert_eq!(chapters[0].title.as_deref(), Some("Prologue"));
		assert_eq!(chapters[1].chapter_number, Some(1.0));
		assert_eq!(chapters[1].title.as_deref(), Some("The Void"));
		// Seconds, between 2020 and 2100.
		assert!(chapters.iter().all(|c| {
			c.date_uploaded
				.is_some_and(|d| d > 1_577_836_800 && d < 4_102_444_800)
		}));
		assert!(
			chapters
				.windows(2)
				.all(|w| w[0].chapter_number < w[1].chapter_number)
		);
	}

	#[buny_test]
	fn test_listings() {
		let source = RoyalRoad::new();
		for id in [
			"best-rated",
			"trending",
			"rising-stars",
			"new-releases",
			"latest-updates",
		] {
			let listing = Listing {
				id: id.into(),
				..Default::default()
			};
			let result = source.get_novel_list(listing, 1).unwrap();
			println!("{id}: {} entries", result.entries.len());
			assert!(!result.entries.is_empty(), "{id}");
		}
	}

	#[buny_test]
	fn test_home() {
		let home = RoyalRoad::new().get_home().unwrap();
		for c in &home.components {
			let count = match &c.value {
				HomeComponentValue::Details { entries, .. }
				| HomeComponentValue::Scroller { entries, .. }
				| HomeComponentValue::Stack { entries, .. }
				| HomeComponentValue::Vertical { entries, .. } => entries.len(),
				_ => 0,
			};
			println!("{:?}: {count}", c.title);
			// The Vertical grid is sent empty; the app pages it itself.
			if !matches!(c.value, HomeComponentValue::Vertical { .. }) {
				assert!(count > 0, "{:?}", c.title);
			}
		}
		assert_eq!(home.components.len(), 5);
	}

	#[buny_test]
	fn test_reviews() {
		let novel = Novel {
			key: "21220/mother-of-learning".into(),
			..Default::default()
		};
		let source = RoyalRoad::new();
		let result = source.get_novel_reviews(novel.clone(), 1).unwrap();
		println!(
			"{} reviews, next: {}",
			result.entries.len(),
			result.has_next_page
		);
		if let Some(r) = result.entries.first() {
			println!("{r:?}");
		}
		assert!(!result.entries.is_empty());
		assert!(result.has_next_page);
		for r in &result.entries {
			assert!(!r.author.is_empty());
			assert!(r.content.as_deref().is_some_and(|c| !c.is_empty()));
			assert!(r.rating.is_some_and(|x| (0.0..=1.0).contains(&x)));
			assert!(
				r.date
					.is_some_and(|d| d > 1_262_304_000 && d < 4_102_444_800)
			);
		}
		let past_end = source.get_novel_reviews(novel, 9999).unwrap();
		assert!(past_end.entries.is_empty());
		assert!(!past_end.has_next_page);
	}
}
