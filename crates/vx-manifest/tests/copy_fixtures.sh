#!/bin/bash
# Copy all Cargo.toml files from ~/bearcove/ to fixtures/ with unique names

set -e

FIXTURES_DIR="$(dirname "$0")/fixtures"
BEARCOVE_DIR="$HOME/bearcove"

# Create fixtures directory
mkdir -p "$FIXTURES_DIR"

# Clean old fixtures
rm -f "$FIXTURES_DIR"/*.toml

echo "Copying Cargo.toml files from $BEARCOVE_DIR..."

counter=0

# Find all Cargo.toml files and copy them with unique names
find "$BEARCOVE_DIR" -name "Cargo.toml" -type f | while IFS= read -r file; do
    # Get the relative path from bearcove
    rel_path="${file#$BEARCOVE_DIR/}"

    # Convert path to a safe filename: replace / with __ and remove .toml extension temporarily
    safe_name=$(echo "$rel_path" | sed 's/\//__/g' | sed 's/Cargo\.toml//')

    # Final name: <path>__Cargo.toml
    dest_name="${safe_name}Cargo.toml"

    # Copy the file
    cp "$file" "$FIXTURES_DIR/$dest_name"

    counter=$((counter + 1))

    if [ $((counter % 100)) -eq 0 ]; then
        echo "  Copied $counter files..."
    fi
done

echo "✓ Copied $counter Cargo.toml files to fixtures/"
