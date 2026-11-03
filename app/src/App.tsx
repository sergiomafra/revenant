// The app shell: loads the settings (theme and language) and shows the page. The screens and the routing between
// them come in the next task.
import { Page } from './kit/Page';
import { SettingsProvider } from './settings/SettingsProvider';

export default function App() {
  return (
    <SettingsProvider>
      <div data-testid="app" className="h-full">
        <Page />
      </div>
    </SettingsProvider>
  );
}
