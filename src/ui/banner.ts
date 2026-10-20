import figlet from "figlet";
import gradient from "gradient-string";
import boxen from "boxen";
import chalk from "chalk";

const BANNER_GRADIENT = gradient(["#00d2ff", "#3a7bd5", "#6dd5ed"]);

/**
 * Render the large ASCII art banner with gradient colors inside a box.
 * Falls back to a compact banner if terminal is too narrow.
 */
export function renderBanner(): string {
  const columns = process.stdout.columns || 80;

  // Figlet ANSI Shadow needs ~80 columns. Fall back to compact banner if narrow.
  if (columns < 80) {
    return renderCompactBanner();
  }

  try {
    const asciiArt = figlet.textSync("DEPLOY", {
      font: "ANSI Shadow",
      horizontalLayout: "fitted",
    });

    const coloredArt = BANNER_GRADIENT.multiline(asciiArt);
    const subtitle = chalk.gray("  AWS CodePipeline Manager") + chalk.gray.dim("  v1.0.0");
    const content = `\n${coloredArt}\n\n${subtitle}\n`;

    return boxen(content, {
      borderStyle: "double",
      borderColor: "#3a7bd5",
      padding: { left: 2, right: 2, top: 0, bottom: 0 },
      dimBorder: false,
    });
  } catch {
    return renderCompactBanner();
  }
}

/**
 * Compact banner for narrow terminals or when figlet fails.
 */
function renderCompactBanner(): string {
  const title = BANNER_GRADIENT("DEPLOY CLI");
  const subtitle = chalk.gray("AWS CodePipeline Manager  v1.0.0");
  return boxen(`${title}\n${subtitle}`, {
    borderStyle: "round",
    borderColor: "#3a7bd5",
    padding: { left: 2, right: 2, top: 0, bottom: 0 },
  });
}

/**
 * Render a compact section header with gradient underline.
 */
export function renderSectionHeader(title: string): string {
  const line = "─".repeat(title.length + 4);
  return `\n  ${chalk.bold.white(title)}\n  ${chalk.gray(line)}`;
}

/**
 * Render a status box for deployment results.
 */
export function renderStatusBox(
  content: string,
  borderColor: string = "#3a7bd5"
): string {
  return boxen(content, {
    borderStyle: "round",
    borderColor,
    padding: { left: 1, right: 1, top: 0, bottom: 0 },
  });
}
