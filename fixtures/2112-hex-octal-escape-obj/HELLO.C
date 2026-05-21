int main(void) {
  static char s[] = "\xAB\101\x7F";
  return s[0] + s[1] + s[2];
}
