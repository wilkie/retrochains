int g;
int main(void) {
  if (g) goto end;
  if (g == 1) goto end;
  g = 2;
end:
  return 0;
}
