import { Component } from 'react';
import type { ErrorInfo, ReactNode } from 'react';

interface Props {
  children: ReactNode;
}

interface State {
  error: Error | null;
}

/**
 * Last line of defence: a render crash must never leave a white page.
 */
export class ErrorBoundary extends Component<Props, State> {
  override state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  override componentDidCatch(_error: Error, _info: ErrorInfo): void {
    // Intentionally silent: the fallback below shows the user what happened.
  }

  private reset = (): void => {
    this.setState({ error: null });
  };

  override render(): ReactNode {
    if (this.state.error) {
      return (
        <div className="fatal">
          <div className="fatal__box">
            <h1 className="fatal__title">openEther UI hit an unexpected error</h1>
            <p className="fatal__msg">{this.state.error.message}</p>
            <button type="button" className="btn btn--primary" onClick={this.reset}>
              Try again
            </button>
          </div>
        </div>
      );
    }
    return this.props.children;
  }
}
