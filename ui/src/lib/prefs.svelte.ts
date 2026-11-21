// Per-viewer UI conveniences that persist (collapsed sidebar, theme).
export type Theme = 'light' | 'dark';
const THEME_KEY = 'recalld.theme';
const darkQuery = typeof matchMedia !== 'undefined' ? matchMedia('(prefers-color-scheme: dark)') : null;
const systemTheme = (): Theme => (darkQuery?.matches ? 'dark' : 'light');
/** The theme the viewer chose, or null while they follow the system. */
function chosenTheme(): Theme | null {
  try {
    const v = localStorage.getItem(THEME_KEY);
    return v === 'light' || v === 'dark' ? v : null;
  } catch {
    return null;
  }
}

function read(key: string, fallback: boolean) {
  try {
    const v = localStorage.getItem(key);
    return v === null ? fallback : v === '1';
  } catch {
    return fallback;
  }
}
function write(key: string, v: boolean) {
  try {
    localStorage.setItem(key, v ? '1' : '0');
  } catch {
    /* ignore */
  }
}

class Prefs {
  sidebarCollapsed = $state(read('recalld.sidebarCollapsed', false));
  /** The theme in use (DESIGN §4): the system's, unless the viewer chose the
   *  other one. app.html sets <html data-theme> before first paint. */
  theme = $state<Theme>(chosenTheme() ?? systemTheme());
  constructor() {
    this.applyTheme(this.theme);
    darkQuery?.addEventListener('change', () => {
      if (!chosenTheme()) this.applyTheme(systemTheme());
    });
  }
  toggleSidebar() {
    this.sidebarCollapsed = !this.sidebarCollapsed;
    write('recalld.sidebarCollapsed', this.sidebarCollapsed);
  }
  /** Switch theme. Going back to the system's own theme clears the choice, so
   *  the UI follows the system again. */
  toggleTheme() {
    const next: Theme = this.theme === 'dark' ? 'light' : 'dark';
    try {
      if (next === systemTheme()) localStorage.removeItem(THEME_KEY);
      else localStorage.setItem(THEME_KEY, next);
    } catch {
      /* ignore */
    }
    this.applyTheme(next);
  }
  private applyTheme(t: Theme) {
    if (typeof document !== 'undefined') document.documentElement.dataset.theme = t;
    this.theme = t;
  }
}
export const prefs = new Prefs();
