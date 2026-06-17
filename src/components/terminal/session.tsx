import { type Event, listen } from '@tauri-apps/api/event';
import type { ITerminalDimensions } from '@xterm/addon-fit';
import type { Terminal } from '@xterm/xterm';
import { errorLog, traceLog } from '@/lib/log';
import {
	initializeSession,
	resizeSession,
	updateCurrentSession,
	writeToSession,
} from '@/lib/os';
import { getScrollback } from '@/lib/setting';
import { type Addons, createTerminal } from '@/lib/terminal';
import { useTheme } from '@/lib/themes';
import generateTerminalTheme from '@/lib/themes/terminal';
import { cn } from '@/lib/utils';
import type { TerminalProps } from '@/models';
import '@xterm/xterm/css/xterm.css';
import {
	type Accessor,
	createEffect,
	createSignal,
	on,
	onCleanup,
	onMount,
	Show,
} from 'solid-js';
import ContextMenu from '@/components/terminal/context-menu';
import HistoryPopup from '@/components/terminal/history';
import InputEditor from '@/components/terminal/input-editor';
import SearchBar from '@/components/terminal/search';

/**
 * GCD of screen width and height, used to detect aspect ratio.
 * - GCD 100 = 32:9 ultrawide (e.g. 5120x1440 -> 5120/100=51.2, but actually 3200x900 etc.)
 * - GCD 256 = 16:9 at certain resolutions (e.g. 3840x2160 -> 3840/256=15, 2160/256=8.4...)
 */
const SCREEN_GCD_ULTRAWIDE = 100;
const SCREEN_GCD_16_9 = 256;

/** Extra columns/rows added to fit-addon dimensions to compensate for aspect ratio quirks (see #302) */
const DEFAULT_EXTRA_COLS = 1;
const DEFAULT_EXTRA_ROWS = 0;
const ULTRAWIDE_EXTRA_COLS = 3;
const ULTRAWIDE_EXTRA_ROWS = 1;
const WIDESCREEN_EXTRA_COLS = 2;

/** Screen width breakpoints for terminal font size selection */
const FONT_SIZE_BREAKPOINT_SM = 1920;
const FONT_SIZE_BREAKPOINT_MD = 2560;
const FONT_SIZE_BREAKPOINT_LG = 3840;
const FONT_SIZE_SM = 12;
const FONT_SIZE_MD = 14;
const FONT_SIZE_LG = 16;
const FONT_SIZE_XL = 20;

function gcd(a: number, b: number): number {
	return b === 0 ? a : gcd(b, a % b);
}

async function resize(id: string, term: Terminal, addons: Addons) {
	const fitAddon = addons.fit;
	if (!fitAddon.proposeDimensions()) {
		await errorLog('Fail to get propose dimensions');
		return;
	}
	let { cols, rows } = fitAddon.proposeDimensions() as ITerminalDimensions;

	// Apply custom fixes based on screen ratio, see #302
	const d = gcd(screen.width, screen.height);
	let extraCols = DEFAULT_EXTRA_COLS;
	let extraRows = DEFAULT_EXTRA_ROWS;

	if (d === SCREEN_GCD_ULTRAWIDE) {
		extraCols = ULTRAWIDE_EXTRA_COLS;
		extraRows = ULTRAWIDE_EXTRA_ROWS;
	} else if (d === SCREEN_GCD_16_9) {
		extraCols = WIDESCREEN_EXTRA_COLS;
	}

	cols = cols + extraCols;
	rows = rows + extraRows;

	if (term.cols !== cols || term.rows !== rows) {
		term.resize(cols, rows);
		fitAddon.fit();
		await resizeSession(id, term.rows, term.cols);
	}
}

function useScreenWidth(onResize?: () => void): Accessor<number> {
	const [screenWidth, setScreenWidth] = createSignal(window.innerWidth);

	let rafId = 0;
	const handler = () => {
		if (rafId) return;
		rafId = requestAnimationFrame(() => {
			rafId = 0;
			setScreenWidth(window.innerWidth);
			onResize?.();
		});
	};
	window.addEventListener('resize', handler);
	onCleanup(() => {
		window.removeEventListener('resize', handler);
		if (rafId) cancelAnimationFrame(rafId);
	});

	return screenWidth;
}

const FONT_SIZE_MIN = 8;
const FONT_SIZE_MAX = 32;

interface SessionProps {
	id: string;
	active: Accessor<string>;
	onActivity?: (id: string) => void;
}

function Session({ id, active, onActivity }: SessionProps) {
	const { theme } = useTheme();

	const controller = new AbortController();

	// fontSize — useScreenWidth also handles terminal resize via RAF-debounced callback
	const screenWidth = useScreenWidth(() => resizeTerminal(id));
	const baseFontSize = () => {
		if (screenWidth() < FONT_SIZE_BREAKPOINT_SM) {
			return FONT_SIZE_SM;
		} else if (screenWidth() < FONT_SIZE_BREAKPOINT_MD) {
			return FONT_SIZE_MD;
		} else if (screenWidth() < FONT_SIZE_BREAKPOINT_LG) {
			return FONT_SIZE_LG;
		}
		return FONT_SIZE_XL;
	};

	const [fontSizeOffset, setFontSizeOffset] = createSignal(0);
	const fontSize = () =>
		Math.max(
			FONT_SIZE_MIN,
			Math.min(FONT_SIZE_MAX, baseFontSize() + fontSizeOffset()),
		);

	const [showSearch, setShowSearch] = createSignal(false);
	const [showHistory, setShowHistory] = createSignal(false);
	const [editorMode, setEditorMode] = createSignal(true);
	const [contextMenu, setContextMenu] = createSignal<{
		x: number;
		y: number;
	} | null>(null);

	let terminalEl: HTMLDivElement | undefined;
	let terminal: TerminalProps | undefined;

	async function resizeTerminal(id: string) {
		if (terminal) {
			await resize(id, terminal.term, terminal.addons);
		}
	}

	onMount(async () => {
		try {
			await traceLog(`Initialize terminal interface. Id: ${id}`);
			if (!terminalEl) {
				await errorLog(
					'terminalEl is undefined in onMount, this should not happen',
				);
				return;
			}
			const scrollback = await getScrollback();
			terminal = await createTerminal(
				terminalEl,
				theme(),
				fontSize(),
				scrollback,
			);

			await initializeSession(id);

			await resize(id, terminal.term, terminal.addons);

			// In editor mode, xterm.js onData only fires for raw mode keystrokes.
			// In raw mode (interactive programs), all keystrokes pass through.
			terminal.term.onData(v => {
				if (!editorMode()) {
					writeToSession(id, v);
				}
			});

			// Copy on select — debounced to avoid clipboard spam during drag
			const term = terminal.term;
			let clipboardTimer: ReturnType<typeof setTimeout> | undefined;
			term.onSelectionChange(() => {
				clearTimeout(clipboardTimer);
				clipboardTimer = setTimeout(() => {
					const sel = term.getSelection();
					if (sel) {
						navigator.clipboard.writeText(sel);
					}
				}, 100);
			});

			// Visual bell flash
			term.onBell(() => {
				if (terminalEl) {
					terminalEl.classList.add('bell-flash');
					setTimeout(() => terminalEl?.classList.remove('bell-flash'), 200);
				}
			});

			terminal.term.focus();
		} catch (e) {
			await errorLog(e);
		}
	});

	// refocus on tab change — focus editor in editor mode, xterm in raw mode
	createEffect(
		on(active, async active => {
			try {
				if (active === id) {
					await resizeTerminal(id);
					if (!editorMode()) {
						terminal?.term.focus();
					}
					// In editor mode, InputEditor handles its own focus via visible effect
					await updateCurrentSession(id);
				} else {
					terminal?.term.blur();
				}
			} catch (e) {
				await errorLog(e);
			}
		}),
	);

	// sync terminal theme
	createEffect(
		on(theme, async theme => {
			if (terminal?.term) {
				terminal.term.options = { ...generateTerminalTheme(theme) };
			}
		}),
	);

	// sync terminal font size
	createEffect(
		on(fontSize, async fontSize => {
			if (terminal?.term) {
				terminal.term.options.fontSize = fontSize;
			}
		}),
	);

	// OSC 133 shell integration marker strings for auto-switching editor/raw mode.
	// B = prompt ready (editor mode), C = command started (raw mode)
	const OSC_133_B_BEL = '\x1b]133;B\x07';
	const OSC_133_B_ST = '\x1b]133;B\x1b\\';
	const OSC_133_C_BEL = '\x1b]133;C\x07';
	const OSC_133_C_ST = '\x1b]133;C\x1b\\';

	// Batch PTY output to reduce xterm.js write() calls during output floods.
	// We schedule the flush via BOTH requestAnimationFrame and a setTimeout
	// fallback: WebKitGTK (Tauri's Linux webview) throttles RAF callbacks when
	// the page is otherwise idle, so RAF alone would stall buffered output until
	// the next input event woke the compositor — making each keystroke's output
	// in interactive apps (vim, top) appear only after the *next* keystroke.
	// The setTimeout fires reliably even when idle; whichever runs first flushes.
	let pendingData = '';
	let flushScheduled = false;
	let rafId = 0;
	let timeoutId: ReturnType<typeof setTimeout> | undefined;

	function flush() {
		flushScheduled = false;
		if (rafId) {
			cancelAnimationFrame(rafId);
			rafId = 0;
		}
		if (timeoutId !== undefined) {
			clearTimeout(timeoutId);
			timeoutId = undefined;
		}
		if (pendingData) {
			terminal?.term.write(pendingData);
			pendingData = '';
		}
	}

	const unListen = listen(`data-${id}`, (e: Event<string>) => {
		const payload = e.payload;

		// Detect OSC 133 markers for auto mode switching
		if (payload.includes(OSC_133_B_BEL) || payload.includes(OSC_133_B_ST)) {
			setEditorMode(true);
		}
		if (payload.includes(OSC_133_C_BEL) || payload.includes(OSC_133_C_ST)) {
			setEditorMode(false);
		}

		pendingData += payload;
		if (!flushScheduled) {
			flushScheduled = true;
			rafId = requestAnimationFrame(flush);
			timeoutId = setTimeout(flush, 8);
		}
		if (active() !== id && onActivity) {
			onActivity(id);
		}
	});

	function handleKeyboardShortcuts(e: KeyboardEvent) {
		if (active() !== id) return;

		if (e.ctrlKey && e.key === 'f') {
			e.preventDefault();
			setShowSearch(true);
		}
		if (e.ctrlKey && e.shiftKey && e.key === 'H') {
			e.preventDefault();
			setShowHistory(true);
		}

		// Toggle editor/raw mode
		if (e.ctrlKey && e.shiftKey && e.key === 'E') {
			e.preventDefault();
			setEditorMode(prev => !prev);
			if (!editorMode()) {
				terminal?.term.focus();
			}
			return;
		}

		// Clear scrollback
		if (e.ctrlKey && e.shiftKey && e.key === 'K') {
			e.preventDefault();
			terminal?.term.clear();
		}

		// Font size zoom
		if (e.ctrlKey && (e.key === '=' || e.key === '+')) {
			e.preventDefault();
			if (fontSize() < FONT_SIZE_MAX) {
				setFontSizeOffset(prev => prev + 1);
				resizeTerminal(id);
			}
		}
		if (e.ctrlKey && e.key === '-') {
			e.preventDefault();
			if (fontSize() > FONT_SIZE_MIN) {
				setFontSizeOffset(prev => prev - 1);
				resizeTerminal(id);
			}
		}
		if (e.ctrlKey && e.key === '0') {
			e.preventDefault();
			setFontSizeOffset(0);
			resizeTerminal(id);
		}
	}

	window.addEventListener('keydown', handleKeyboardShortcuts, {
		signal: controller.signal,
	});

	onCleanup(() => {
		if (rafId) cancelAnimationFrame(rafId);
		if (timeoutId !== undefined) clearTimeout(timeoutId);
		terminal?.term.dispose();
		unListen.then(f => f()).catch(errorLog);
		controller.abort();
	});

	return (
		<div
			class={cn(
				active() !== id && 'hidden',
				'relative flex size-full flex-col p-2',
			)}
			onContextMenu={e => {
				e.preventDefault();
				setContextMenu({ x: e.clientX, y: e.clientY });
			}}
		>
			<Show when={showSearch() && terminal?.addons.search}>
				{addon => (
					<SearchBar
						searchAddon={addon()}
						onClose={() => {
							setShowSearch(false);
							terminal?.term.focus();
						}}
					/>
				)}
			</Show>
			<Show when={showHistory() && terminal}>
				<HistoryPopup
					onSelect={(cmd: string) => {
						writeToSession(id, cmd);
						setShowHistory(false);
						if (editorMode()) {
							// Focus stays on editor
						} else {
							terminal?.term.focus();
						}
					}}
					onClose={() => {
						setShowHistory(false);
						terminal?.term.focus();
					}}
				/>
			</Show>
			<Show when={contextMenu() && terminal ? contextMenu() : null}>
				{pos => (
					<ContextMenu
						x={pos().x}
						y={pos().y}
						terminal={terminal?.term as Terminal}
						sessionId={id}
						onClose={() => {
							setContextMenu(null);
							terminal?.term.focus();
						}}
						onSearch={() => setShowSearch(true)}
						onHistory={() => setShowHistory(true)}
					/>
				)}
			</Show>
			<div class="min-h-0 flex-1" ref={el => (terminalEl = el)} />
			<InputEditor
				theme={theme}
				fontSize={fontSize}
				visible={editorMode}
				onSubmit={text => {
					writeToSession(id, `${text}\n`);
				}}
				onRawKey={key => {
					writeToSession(id, key);
				}}
			/>
		</div>
	);
}

export default Session;
