import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { BrowserRouter } from 'react-router-dom';

import '@otwono/ui/tokens.css';
import '@otwono/ui/base.css';
import './styles/app.css';

import { App } from './App';
import { bootstrapRuntime } from './bootstrap';

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      retry: (failureCount, error) => {
        // A refusal or a missing thing will not become true by asking again.
        const status = (error as { status?: number })?.status ?? 0;
        if (status >= 400 && status < 500) return false;
        return failureCount < 2;
      },
      staleTime: 5_000,
    },
  },
});

const container = document.getElementById('root');
if (!container) throw new Error('The application root element is missing.');

const root = createRoot(container);

function renderStartupFailure(message: string) {
  root.render(
    <div className="screen screen--centered">
      <div className="notice notice--negative" role="alert">
        <div className="notice__body">
          <strong className="notice__title">OTWONO could not start</strong>
          <div>{message}</div>
        </div>
      </div>
    </div>,
  );
}

bootstrapRuntime()
  .then(() => {
    root.render(
      <StrictMode>
        <QueryClientProvider client={queryClient}>
          <BrowserRouter>
            <App />
          </BrowserRouter>
        </QueryClientProvider>
      </StrictMode>,
    );
  })
  .catch((error: unknown) => {
    renderStartupFailure(error instanceof Error ? error.message : String(error));
  });
