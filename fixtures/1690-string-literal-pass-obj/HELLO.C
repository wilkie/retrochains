int sum_chars(char *s) {
  int n = 0;
  while (*s) {
    n += *s;
    s++;
  }
  return n;
}
int main(void) {
  return sum_chars("ABC");
}
