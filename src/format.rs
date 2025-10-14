// Copyright (c) 2025 Marceline Cramer
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Pomidor is free software: you can redistribute it and/or modify it under
// the terms of the GNU Affero General Public License as published by the Free
// Software Foundation, either version 3 of the License, or (at your option) any
// later version.
//
// Pomidor is distributed in the hope that it will be useful, but WITHOUT ANY
// WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS
// FOR A PARTICULAR PURPOSE.  See the GNU Affero General Public License for
// more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with Pomidor. If not, see <https://www.gnu.org/licenses/>.

//! This module uses [f64] as the time type because the [minijinja_contrib]
//! crate uses it as an internal time representation and because f64
//! supports simple arithmetic within templates.

use anyhow::Context;
use minijinja::Environment;
use serde::Serialize;

use crate::protocol::*;

/// The context for formatting contexts.
#[derive(Clone, Debug, Serialize)]
pub struct TemplateContext {
    /// The time this timer entered this mode, in Unix timestamp.
    pub started: f64,

    /// An optional deadline for this mode, in Unix timestamp.
    pub deadline: Option<f64>,

    /// The number of short breaks remaining before a long break.
    pub short_breaks: u8,

    /// The [TimerMode] formatted as CamelCase.
    pub mode: String,
}

impl From<&TimerState> for TemplateContext {
    fn from(state: &TimerState) -> Self {
        TemplateContext {
            started: state.started.timestamp() as f64,
            deadline: state.deadline.map(|deadline| deadline.timestamp() as f64),
            short_breaks: state.short_breaks,
            mode: format!("{:?}", state.mode), // use debug formatting for easier debugging
        }
    }
}

/// A template formatting engine.
///
/// Reuse when possible because this is difficult to construct.
pub struct Formatter {
    environment: Environment<'static>,
}

impl Formatter {
    /// Creates a new template formatting engine.
    pub fn new() -> Self {
        // initialize environment with built-in filters and extensions
        let mut environment = Environment::new();

        // add custom filters and extensions to environment
        // datetime formatting is most important
        minijinja_contrib::add_to_environment(&mut environment);

        // set default time formatting to "MM:SS"
        environment.add_global("TIME_FORMAT", "[minute]:[second]");

        Self { environment }
    }

    /// Attempts to create a [Template] from a template string.
    pub fn make_template<'a>(&'a self, src: &'a str) -> anyhow::Result<Template<'a>> {
        let template = self
            .environment
            .template_from_str(src)
            .context("failed to compile template")?;

        Ok(Template { template })
    }
}

/// A template that can render [TimerState] into a formatted string.
pub struct Template<'a> {
    template: minijinja::Template<'a, 'a>,
}

impl<'a> Template<'a> {
    /// Renders the template with the given [TimerState].
    pub fn render(&self, state: &TimerState) -> String {
        let context = TemplateContext::from(state);
        self.template.render(&context).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::{Duration, Local};

    fn render(template: &str, state: &TimerState) -> String {
        let formatter = Formatter::new();
        let template = formatter.make_template(template).unwrap();
        template.render(state)
    }

    fn state_from_duration(minutes: i64, seconds: i64) -> TimerState {
        let started = Local::now();
        let deadline = started + Duration::minutes(minutes) + Duration::seconds(seconds);

        TimerState {
            started,
            deadline: Some(deadline),
            short_breaks: 0,
            mode: TimerMode::Focus,
        }
    }

    #[test]
    fn test_format_duration() {
        let template = "{{ (deadline - started) | timeformat }}";
        let state = state_from_duration(4, 20);
        assert_eq!(render(template, &state), "04:20");
    }
}
