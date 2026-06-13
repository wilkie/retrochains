int main(void) {
  char c = 'X';
  switch (c) {
    case 'A':
    case 'B':
    case 'C':
      return 1;
    case 'X':
    case 'Y':
      return 2;
  }
  return 0;
}
