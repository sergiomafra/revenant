import { render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import App from './App';
import { fakeBackend } from './test-utils';

describe('App', () => {
  it('renders the app root once the settings are loaded', async () => {
    fakeBackend({ get_settings: () => ({ theme: 'dark', language: 'es' }) });
    render(<App />);
    expect(await screen.findByTestId('app')).toBeInTheDocument();
    expect(document.documentElement).toHaveAttribute('data-theme', 'dark');
    expect(document.documentElement).toHaveAttribute('lang', 'es');
  });
});
