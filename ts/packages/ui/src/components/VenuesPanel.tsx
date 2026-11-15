import type { VenueHealth } from '../api/types';
import { fmtUsdPrefixed } from '../lib/format';
import { Panel } from './Panel';

interface VenuesPanelProps {
  venues: VenueHealth[];
}

interface VenueState {
  label: string;
  tone: 'live' | 'paper' | 'readonly' | 'unarmed';
}

function stateOf(venue: VenueHealth): VenueState {
  if (venue.venue === 'paper') return { label: 'simulated', tone: 'paper' };
  if (!venue.tradable) return { label: 'read-only', tone: 'readonly' };
  return venue.live ? { label: 'live', tone: 'live' } : { label: 'not armed', tone: 'unarmed' };
}

/**
 * Every venue the core may route to, with the cap the risk layer applies to it.
 *
 * The cap is the effective one: the smaller of the configured limit and the
 * venue's own arming record, so a tightened venue shows a tighter number here
 * than the risk panel's configured limit.
 */
export function VenuesPanel({ venues }: VenuesPanelProps) {
  return (
    <Panel title="Venues" hint={`${venues.length} configured`} bodyClassName="panel__bd--flush">
      {venues.length === 0 ? (
        <div className="empty">
          <p className="empty__title">No venues reported</p>
          <p className="empty__hint">The core has not answered `/api/venues` yet.</p>
        </div>
      ) : (
        <ul className="venues">
          {venues.map((venue) => {
            const state = stateOf(venue);
            return (
              <li key={venue.venue} className="venues__row">
                <span
                  className={`venues__dot venues__dot--${venue.healthy ? 'ok' : 'bad'}`}
                  title={venue.healthy ? 'the venue answers' : 'the venue did not answer'}
                  aria-hidden="true"
                />
                <span className="venues__name">{venue.venue}</span>
                <span className={`venues__state venues__state--${state.tone}`}>{state.label}</span>
                {venue.active ? (
                  <span className="venues__book" title="The book is reported under this venue">
                    book
                  </span>
                ) : null}
                <span className="venues__cap num" title="Largest single order the risk layer allows here">
                  {fmtUsdPrefixed(venue.max_order_notional_usd)}/order
                </span>
              </li>
            );
          })}
        </ul>
      )}
    </Panel>
  );
}
