import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import { SettingsProvider } from '../settings/SettingsProvider';
import { fakeBackend } from '../test-utils';
import Settings from './Settings';

function open(stored = { theme: 'auto', language: 'pt-BR' }) {
  const calls = fakeBackend({ get_settings: () => stored, set_settings: () => null });
  render(
    <SettingsProvider>
      <Settings onClose={() => {}} />
    </SettingsProvider>,
  );
  return calls;
}

describe('Settings', () => {
  it('switching the theme to dark stores it and applies it to <html>', async () => {
    const calls = open();
    await screen.findByRole('dialog', { name: 'Ajustes' });
    expect(document.documentElement).not.toHaveAttribute('data-theme');
    fireEvent.click(screen.getByRole('radio', { name: 'Escuro' }));
    await waitFor(() =>
      expect(calls).toContainEqual({ cmd: 'set_settings', args: { settings: { theme: 'dark', language: 'pt-BR' } } }),
    );
    expect(document.documentElement).toHaveAttribute('data-theme', 'dark');
    expect(screen.getByRole('radio', { name: 'Escuro' })).toBeChecked();
  });

  it('switching the language to es shows the screen in Spanish', async () => {
    const calls = open();
    await screen.findByRole('button', { name: 'Concluir' });
    fireEvent.click(screen.getByRole('radio', { name: /Español/ }));
    expect(await screen.findByRole('button', { name: 'Listo' })).toBeInTheDocument();
    expect(screen.getByText('Automático sigue el tema de tu computadora.')).toBeInTheDocument();
    expect(screen.queryByText('Concluir')).not.toBeInTheDocument();
    await waitFor(() =>
      expect(calls).toContainEqual({ cmd: 'set_settings', args: { settings: { theme: 'auto', language: 'es' } } }),
    );
  });

  it('says so when the choice cannot be stored', async () => {
    fakeBackend({
      get_settings: () => ({ theme: 'auto', language: 'pt-BR' }),
      set_settings: () => {
        throw { code: 'settings_write_failed' };
      },
    });
    render(
      <SettingsProvider>
        <Settings onClose={() => {}} />
      </SettingsProvider>,
    );
    await screen.findByRole('dialog', { name: 'Ajustes' });
    expect(screen.queryByText(/Não deu para guardar a escolha/)).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('radio', { name: 'Escuro' }));
    expect(
      await screen.findByText('Não deu para guardar a escolha. Ela vale até você fechar o Revenant.'),
    ).toBeInTheDocument();
    expect(document.documentElement).toHaveAttribute('data-theme', 'dark');
  });
});
