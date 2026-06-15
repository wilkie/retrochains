int is_letter(char c) {
  if (c >= 'a' && c <= 'z') return 1;
  if (c >= 'A' && c <= 'Z') return 1;
  return 0;
}
int main(void) {
  return is_letter('M');
}
