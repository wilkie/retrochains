int g;
int main(void) {
  if (g) {
    if (g == 1) goto end;
    g = g + 1;
  }
  g = g + 2;
end:
  return 0;
}
