import { Routes } from '@angular/router';

/**
 * `data.bare` hides the app shell (header + nav) — used by `browser-view`,
 * which is meant to be embedded (companion extension, side panels).
 *
 * **Order matters.** The messenger layout lives on an empty-path route with
 * children (`''` → the chat list, `chat/:jid` → a conversation). An empty-path
 * parent matches *any* URL, and when none of its children match, the router
 * does not fall back to the routes after it — the URL ends up on the `**`
 * wildcard instead. So every full-page route is declared **before** the shell;
 * otherwise `/login`, `/settings` and `/about` silently redirect to `/`.
 */
export const routes: Routes = [
  {
    path: 'group',
    loadChildren: () => import('./features/groups/groups.routes').then((m) => m.GROUP_ROUTES),
  },
  {
    path: 'settings',
    title: 'Settings',
    loadComponent: () => import('./features/settings/settings.component').then((m) => m.SettingsComponent),
  },
  {
    path: 'login',
    title: 'Login',
    loadComponent: () => import('./features/auth/login.component').then((m) => m.LoginComponent),
  },
  {
    path: 'about',
    title: 'About',
    loadComponent: () => import('./features/about/about.component').then((m) => m.AboutComponent),
  },
  {
    path: 'browser-view',
    title: 'Browser view',
    data: { bare: true },
    loadComponent: () => import('./features/browser-view/browser-view.component').then((m) => m.BrowserViewComponent),
  },
  {
    // The two-pane messenger layout: the chat list stays on the left while a
    // conversation fills the pane on the right (`chat/:jid`).
    path: '',
    loadComponent: () => import('./features/chats/chats-shell.component').then((m) => m.ChatsShellComponent),
    children: [
      {
        path: '',
        pathMatch: 'full',
        title: 'Chats',
        loadComponent: () => import('./features/chats/chats-shell.component').then((m) => m.NoChatComponent),
      },
      {
        path: 'chat/:jid',
        title: 'Chat',
        loadComponent: () => import('./features/chats/chat.component').then((m) => m.ChatComponent),
      },
    ],
  },
  { path: '**', redirectTo: '' },
];
