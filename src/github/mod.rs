mod api;

use self::api::{GitHub, TeamPrivacy, TeamRole};
use crate::{github::api::RepoPermission, TeamApi};
use failure::Error;
use log::{debug, info};
use rust_team_data::v1::Bot;
use std::collections::{HashMap, HashSet};

static DEFAULT_DESCRIPTION: &str = "Managed by the rust-lang/team repository.";
static DEFAULT_PRIVACY: TeamPrivacy = TeamPrivacy::Closed;

pub(crate) struct SyncGitHub {
    github: GitHub,
    teams: Vec<rust_team_data::v1::Team>,
    repos: Vec<rust_team_data::v1::Repo>,
    usernames_cache: HashMap<usize, String>,
    org_owners: HashMap<String, HashSet<usize>>,
}

impl SyncGitHub {
    pub(crate) fn new(token: String, team_api: &TeamApi, dry_run: bool) -> Result<Self, Error> {
        let github = GitHub::new(token, dry_run);
        let teams = team_api.get_teams()?;
        let repos = team_api.get_repos()?;

        debug!("caching mapping between user ids and usernames");
        let users = teams
            .iter()
            .filter_map(|t| t.github.as_ref().map(|gh| &gh.teams))
            .flatten()
            .flat_map(|team| &team.members)
            .copied()
            .collect::<HashSet<_>>();
        let usernames_cache = github.usernames(&users.into_iter().collect::<Vec<_>>())?;

        debug!("caching organization owners");
        let orgs = teams
            .iter()
            .filter_map(|t| t.github.as_ref())
            .flat_map(|gh| &gh.teams)
            .map(|gh_team| &gh_team.org)
            .collect::<HashSet<_>>();
        let mut org_owners = HashMap::new();
        for org in &orgs {
            org_owners.insert((*org).to_string(), github.org_owners(org)?);
        }

        Ok(SyncGitHub {
            github,
            teams,
            repos,
            usernames_cache,
            org_owners,
        })
    }

    pub(crate) fn synchronize_all(&self) -> Result<(), Error> {
        for team in &self.teams {
            if let Some(gh) = &team.github {
                for github_team in &gh.teams {
                    self.synchronize_team(github_team)?;
                }
            }
        }

        for repo in &self.repos {
            self.synchronize_repo(repo)?;
        }

        Ok(())
    }

    fn synchronize_team(&self, github_team: &rust_team_data::v1::GitHubTeam) -> Result<(), Error> {
        let slug = format!("{}/{}", github_team.org, github_team.name);
        debug!("synchronizing team {}", slug);

        // Ensure the team exists and is consistent
        let team = match self.github.team(&github_team.org, &github_team.name)? {
            Some(team) => team,
            None => self.github.create_team(
                &github_team.org,
                &github_team.name,
                DEFAULT_DESCRIPTION,
                DEFAULT_PRIVACY,
            )?,
        };
        if team.name != github_team.name
            || team.description != DEFAULT_DESCRIPTION
            || team.privacy != DEFAULT_PRIVACY
        {
            self.github.edit_team(
                &team,
                &github_team.name,
                DEFAULT_DESCRIPTION,
                DEFAULT_PRIVACY,
            )?;
        }

        let mut current_members = self.github.team_memberships(&team)?;

        // Ensure all expected members are in the team
        for member in &github_team.members {
            let expected_role = self.expected_role(&github_team.org, *member);
            let username = &self.usernames_cache[member];
            if let Some(member) = current_members.remove(member) {
                if member.role != expected_role {
                    info!(
                        "{}: user {} has the role {} instead of {}, changing them...",
                        slug, username, member.role, expected_role
                    );
                    self.github.set_membership(&team, username, expected_role)?;
                } else {
                    debug!("{}: user {} is in the correct state", slug, username);
                }
            } else {
                info!("{}: user {} is missing, adding them...", slug, username);
                // If the user is not a member of the org and they *don't* have a pending
                // invitation this will send the invite email and add the membership in a "pending"
                // state.
                //
                // If the user didn't accept the invitation yet the next time the tool runs, the
                // method will be called again. Thankfully though in that case GitHub doesn't send
                // yet another invitation email to the user, but treats the API call as a noop, so
                // it's safe to do it multiple times.
                self.github.set_membership(&team, username, expected_role)?;
            }
        }

        // The previous cycle removed expected members from current_members, so it only contains
        // members to delete now.
        for member in current_members.values() {
            info!(
                "{}: user {} is not in the team anymore, removing them...",
                slug, member.username
            );
            self.github.remove_membership(&team, &member.username)?;
        }

        Ok(())
    }

    fn synchronize_repo(&self, expected_repo: &rust_team_data::v1::Repo) -> Result<(), Error> {
        debug!(
            "synchronizing repo {}/{}",
            expected_repo.org, expected_repo.name
        );

        // Ensure the repo exists or create it.
        let (actual_repo, just_created) =
            match self.github.repo(&expected_repo.org, &expected_repo.name)? {
                Some(r) => {
                    debug!("repo already exists...");
                    (r, false)
                }
                None => {
                    let repo = self.github.create_repo(
                        &expected_repo.org,
                        &expected_repo.name,
                        &expected_repo.description,
                    )?;
                    (repo, true)
                }
            };

        // Ensure the repo is consistent between its expected state and current state
        if !just_created {
            if actual_repo.description != expected_repo.description {
                self.github
                    .edit_repo(actual_repo, &expected_repo.description)?;
            } else {
                debug!("repo is in synced state");
            }
        }

        let mut actual_teams = self
            .github
            .get_teams(&expected_repo.org, &expected_repo.name)?;
        // Sync team and bot permissions
        for expected_team in &expected_repo.teams {
            use rust_team_data::v1;
            let permission = match &expected_team.permission {
                v1::RepoPermission::Write => RepoPermission::Write,
                v1::RepoPermission::Admin => RepoPermission::Admin,
                v1::RepoPermission::Maintain => RepoPermission::Maintain,
            };
            actual_teams.remove(&expected_team.name);
            self.github.update_team_repo_permissions(
                &expected_repo.org,
                &expected_repo.name,
                &expected_team.name,
                &permission,
            )?;
        }

        for bot in &expected_repo.bots {
            let bot_name = match bot {
                Bot::Bors => "bors",
                Bot::Highfive => "rust-highfive",
                Bot::RustTimer => "rust-timer",
                Bot::Rustbot => "rustbot",
            };
            actual_teams.remove(bot_name);
            self.github.update_user_repo_permissions(
                &expected_repo.org,
                &expected_repo.name,
                bot_name,
                &RepoPermission::Write,
            )?;
        }

        // `actual_teams` now contains the teams that were not expected
        // but are still on GitHub. We now remove them.
        for team in &actual_teams {
            self.github
                .remove_team_from_repo(&expected_repo.org, &expected_repo.name, team)?;
        }
        Ok(())
    }

    fn expected_role(&self, org: &str, user: usize) -> TeamRole {
        if let Some(true) = self
            .org_owners
            .get(org)
            .map(|owners| owners.contains(&user))
        {
            TeamRole::Maintainer
        } else {
            TeamRole::Member
        }
    }
}
