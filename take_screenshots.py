import os
import time
from playwright.sync_api import sync_playwright

def main():
    screenshot_dir = r"d:\Projects\klayer\klayer\docs\screenshots"
    os.makedirs(screenshot_dir, exist_ok=True)
    
    with sync_playwright() as p:
        print("Launching Chromium...")
        browser = p.chromium.launch(headless=True)
        # 1280x800 is a standard resolution that fits the layout well
        context = browser.new_context(viewport={"width": 1280, "height": 800})
        page = context.new_page()
        
        # Navigate to dashboard
        url = "http://localhost:7474"
        print(f"Navigating to {url}...")
        page.goto(url)
        
        # Wait for the klayer API connection to be established (live status dot)
        page.wait_for_selector(".status-dot.live", timeout=10000)
        print("Connected to klayer server!")
        
        # Wait a brief moment for the initial data fetch and render
        time.sleep(2)
        
        # Map of (page_id, output_filename)
        pages = [
            ("overview", "dashboard.png"),
            ("domains", "domains.png"),
            ("knowledge", "knowledge.png"),
            ("sources", "sources.png"),
            ("memory", "agent-memory.png"),
            ("trust", "trust-lifecycle.png"),
            ("codebase", "codebase.png"),
            ("settings", "settings.png"),
        ]
        
        for page_id, filename in pages:
            print(f"Capturing page: {page_id}...")
            # Click the sidebar button corresponding to the page
            page.click(f'.nav-btn[data-page="{page_id}"]')
            
            # Wait for content to render and transition
            time.sleep(1)
            
            # Save screenshot
            out_path = os.path.join(screenshot_dir, filename)
            page.screenshot(path=out_path)
            print(f"Saved screenshot: {out_path}")
            
        browser.close()
        print("Done capturing screenshots!")

if __name__ == "__main__":
    main()
