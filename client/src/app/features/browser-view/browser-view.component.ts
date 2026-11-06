import { Component } from '@angular/core';

import { SearchBarComponent } from '../search/search-bar.component';

/**
 * Bare route (`data.bare`): no header / nav, meant to be embedded in a side
 * panel or the companion extension.
 */
@Component({
  selector: 'app-browser-view',
  imports: [SearchBarComponent],
  template: `
    <section class="browser-view">
      <app-search-bar placeholder="Search WhatsApp messages" />
    </section>
  `,
})
export class BrowserViewComponent {}
