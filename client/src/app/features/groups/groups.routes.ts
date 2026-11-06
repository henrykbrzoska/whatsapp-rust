import { Routes } from '@angular/router';

/**
 * Group routes, mounted by `app.routes.ts` under `group`:
 * `{ path: 'group', loadChildren: … GROUP_ROUTES }`.
 * `jid` binds through `withComponentInputBinding()` (enabled in `app.config.ts`).
 */
export const GROUP_ROUTES: Routes = [
  {
    path: 'new',
    title: 'New group',
    loadComponent: () => import('./group-create.component').then((m) => m.GroupCreateComponent),
  },
  {
    path: ':jid',
    title: 'Group info',
    loadComponent: () => import('./group-info.component').then((m) => m.GroupInfoComponent),
  },
];
