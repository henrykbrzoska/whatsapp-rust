import {
  ApplicationConfig,
  inject,
  provideAppInitializer,
  provideBrowserGlobalErrorListeners,
  provideZoneChangeDetection,
} from '@angular/core';
import { provideRouter, withComponentInputBinding } from '@angular/router';

import { routes } from './app.routes';
import { TauriService } from './core/services/tauri.service';

export const appConfig: ApplicationConfig = {
  providers: [
    provideBrowserGlobalErrorListeners(),
    provideZoneChangeDetection({ eventCoalescing: true }),
    provideRouter(routes, withComponentInputBinding()),
    // Runs before the initial navigation, so the `?engine=` handshake has
    // stored the token and cleaned the address bar before the router reads it.
    provideAppInitializer(() => {
      inject(TauriService);
    }),
  ],
};
