import type { ITerminalOptions } from '@xterm/xterm';
import { selectStyle, type Theme } from '@/lib/themes/styles';

// Standard Tango ANSI palette used as the fallback when a theme does not
// define an explicit color. Kept as real colors (not desaturated) so that
// interactive apps (vim, mc, ls) render with recognizable colors.
const DEFAULT_ANSI = {
	black: '#2e3436',
	red: '#cc0000',
	green: '#4e9a06',
	yellow: '#c4a000',
	blue: '#3465a4',
	magenta: '#75507b',
	cyan: '#06989a',
	white: '#d3d7cf',
	brightBlack: '#555753',
	brightRed: '#ef2929',
	brightGreen: '#8ae234',
	brightYellow: '#fce94f',
	brightBlue: '#729fcf',
	brightMagenta: '#ad7fa8',
	brightCyan: '#34e2e2',
	brightWhite: '#eeeeec',
} as const;

export default function generateTerminalTheme(theme: Theme): ITerminalOptions {
	const style = selectStyle(theme);
	return {
		allowProposedApi: true,
		cursorBlink: true,
		cursorStyle: style.terminal.cursorStyle,
		allowTransparency: false,
		fontFamily: `"${style.terminal.fontFamily}", monospace`,
		fontWeight: 'normal',
		fontWeightBold: 'bold',
		letterSpacing: 0,
		lineHeight: 1,
		// scrollback is set per-session via createTerminal(), not here
		theme: {
			foreground: style.terminal.foreground,
			background: style.terminal.background,
			cursor: style.terminal.cursor,
			cursorAccent: style.terminal.cursorAccent,
			black: style.colors.black || DEFAULT_ANSI.black,
			red: style.colors.red || DEFAULT_ANSI.red,
			green: style.colors.green || DEFAULT_ANSI.green,
			yellow: style.colors.yellow || DEFAULT_ANSI.yellow,
			blue: style.colors.blue || DEFAULT_ANSI.blue,
			magenta: style.colors.magenta || DEFAULT_ANSI.magenta,
			cyan: style.colors.cyan || DEFAULT_ANSI.cyan,
			white: style.colors.white || DEFAULT_ANSI.white,
			brightBlack: style.colors.brightBlack || DEFAULT_ANSI.brightBlack,
			brightRed: style.colors.brightRed || DEFAULT_ANSI.brightRed,
			brightGreen: style.colors.brightGreen || DEFAULT_ANSI.brightGreen,
			brightYellow: style.colors.brightYellow || DEFAULT_ANSI.brightYellow,
			brightBlue: style.colors.brightBlue || DEFAULT_ANSI.brightBlue,
			brightMagenta: style.colors.brightMagenta || DEFAULT_ANSI.brightMagenta,
			brightCyan: style.colors.brightCyan || DEFAULT_ANSI.brightCyan,
			brightWhite: style.colors.brightWhite || DEFAULT_ANSI.brightWhite,
		},
	};
}
