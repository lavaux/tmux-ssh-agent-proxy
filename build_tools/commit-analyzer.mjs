// commit-analyzer.mjs — @semantic-release/commit-analyzer, capped at patch on
// release branches. Managed by releaser: edit template/ in releaser, not here.
//
// A release/X.Y branch carries the X.Y line and nothing else. semantic-release
// does not enforce that on its own: the newest line is a plain "release" branch
// whose range is open-ended, so a feat pushed there publishes X.(Y+1).0, and a
// BREAKING CHANGE publishes (X+1).0.0. Older lines are maintenance branches with
// range X.Y.x, where the same commit fails the run as out of range instead.
//
// Neither is wanted. New features belong on main and reach a stable line through
// the next `release.sh cut`. What lands on a release branch anyway ships as a
// fix of that line, so the bump is capped here rather than in the commit rules:
// releaseRules cannot depend on the branch, and .releaserc.json is merged
// between branches.
//
// The cap applies only once the line has a release of its own. The first run on
// a freshly cut release/X.Y is the graduation to X.Y.0, and there the last stable
// release is still on the previous line (pre-release tags do not count on a
// stable branch). Capping that run to patch publishes a patch of the OLD line:
// release/2.0 graduated as 1.1.1 instead of 2.0.0. The graduation run therefore
// returns whichever bump lands exactly on X.Y.0, and fails if none does.
//
// Pass-through everywhere else, with the same options as the plugin it wraps.

import { analyzeCommits as analyze } from "@semantic-release/commit-analyzer";

// The bump taking `last` to exactly major.minor.0, or null if no single bump does.
function graduationBump(last, major, minor) {
  const m = /^(\d+)\.(\d+)\.\d+/.exec(last ?? "");
  if (!m) return major === 1 && minor === 0 ? "major" : null;
  const [lastMajor, lastMinor] = [Number(m[1]), Number(m[2])];
  if (major === lastMajor + 1 && minor === 0) return "major";
  if (major === lastMajor && minor === lastMinor + 1) return "minor";
  return null;
}

export async function analyzeCommits(pluginConfig, context) {
  const type = await analyze(pluginConfig, context);
  const { branch, lastRelease, logger } = context;

  const line = /^release\/(\d+)\.(\d+)$/.exec(branch.name);
  if (!line) return type;

  const [major, minor] = [Number(line[1]), Number(line[2])];
  const last = lastRelease?.version;
  const onLine = last?.startsWith(`${major}.${minor}.`);

  if (!onLine) {
    const bump = graduationBump(last, major, minor);
    if (!bump) {
      throw new Error(
        `Branch ${branch.name} has no ${major}.${minor}.x release yet, and no single ` +
          `bump takes the last release (${last ?? "none"}) to ${major}.${minor}.0.`,
      );
    }
    logger.log(
      "Branch %s graduates from %s to %s.%s.0: using a %s release",
      branch.name,
      last ?? "no release",
      major,
      minor,
      bump,
    );
    return bump;
  }

  if (type === "major" || type === "minor") {
    logger.log(
      "Branch %s only publishes patch releases: capping the %s release to patch",
      branch.name,
      type,
    );
    return "patch";
  }
  return type;
}
