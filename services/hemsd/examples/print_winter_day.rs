//! Print the winter reference day exactly as the landing page shows it.
//!
//! The transcript on the landing page and in the README is held to this report
//! by `the_landing_page_prints_the_day_it_says_it_does`, so when the physics,
//! the tariff or the planner move it, this is what regenerates the copies
//! rather than somebody retyping them.
fn main() {
    let scenario = hemsd::Scenario::winter_with_grid_event(hemsd::HouseholdConfig::default());
    let day = hemsd::run(&scenario).expect("the winter reference day");
    print!("{}", hemsd::render::day(&scenario, &day));
}
