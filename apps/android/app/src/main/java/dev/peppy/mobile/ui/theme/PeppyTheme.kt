package dev.peppy.mobile.ui.theme

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable

@Composable
fun PeppyTheme(content: @Composable () -> Unit) {
    val darkTheme = isSystemInDarkTheme()
    val token = if (darkTheme) PeppyTokens.Dark else PeppyTokens.Light
    val scheme = if (darkTheme) darkColorScheme(
        primary = token.accent, onPrimary = token.accentText,
        primaryContainer = token.surfaceActive, onPrimaryContainer = token.textStrong,
        secondary = token.accentHover, onSecondary = token.accentText,
        secondaryContainer = token.surfaceHover, onSecondaryContainer = token.textPrimary,
        tertiary = token.success, onTertiary = token.surfaceChrome,
        tertiaryContainer = token.surfaceActive, onTertiaryContainer = token.textPrimary,
        background = token.surfaceDesk, onBackground = token.textPrimary,
        surface = token.surfaceChrome, onSurface = token.textPrimary,
        surfaceVariant = token.surfacePanel, onSurfaceVariant = token.textSecondary,
        surfaceTint = token.accent,
        surfaceBright = token.surfaceEditor, surfaceDim = token.surfaceDesk,
        surfaceContainerLowest = token.surfaceEditor, surfaceContainerLow = token.surfaceChrome,
        surfaceContainer = token.surfaceDesk, surfaceContainerHigh = token.surfaceHover,
        surfaceContainerHighest = token.surfaceActive,
        outline = token.borderStrong, outlineVariant = token.border,
        error = token.error, onError = token.surfaceChrome,
        errorContainer = token.surfaceActive, onErrorContainer = token.errorText,
    ) else lightColorScheme(
        primary = token.accent, onPrimary = token.accentText,
        primaryContainer = token.surfaceActive, onPrimaryContainer = token.textStrong,
        secondary = token.accentHover, onSecondary = token.accentText,
        secondaryContainer = token.surfaceHover, onSecondaryContainer = token.textPrimary,
        tertiary = token.success, onTertiary = token.accentText,
        tertiaryContainer = token.surfaceActive, onTertiaryContainer = token.textPrimary,
        background = token.surfaceDesk, onBackground = token.textPrimary,
        surface = token.surfaceChrome, onSurface = token.textPrimary,
        surfaceVariant = token.surfacePanel, onSurfaceVariant = token.textSecondary,
        surfaceTint = token.accent,
        surfaceBright = token.surfaceEditor, surfaceDim = token.surfaceDesk,
        surfaceContainerLowest = token.surfaceEditor, surfaceContainerLow = token.surfaceChrome,
        surfaceContainer = token.surfaceDesk, surfaceContainerHigh = token.surfaceHover,
        surfaceContainerHighest = token.surfaceActive,
        outline = token.borderStrong, outlineVariant = token.border,
        error = token.error, onError = token.accentText,
        errorContainer = token.surfaceActive, onErrorContainer = token.errorText,
    )
    MaterialTheme(colorScheme = scheme, content = content)
}
