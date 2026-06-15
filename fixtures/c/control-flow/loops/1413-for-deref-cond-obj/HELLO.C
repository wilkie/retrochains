int main(void) {
  char *p;
  int n = 0;
  p = "ab";
  for (; *p; p++) n++;
  return n;
}
