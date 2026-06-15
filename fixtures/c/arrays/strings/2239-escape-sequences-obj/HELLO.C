int main(void) {
  static char s[] = "a\nb\tc\0d";
  return s[1] + s[3] + s[5];
}
