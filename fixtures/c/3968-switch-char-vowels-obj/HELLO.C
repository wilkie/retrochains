int classify(char c) {
  switch (c) {
    case 'a': return 1;
    case 'e': return 2;
    case 'i': return 3;
    case 'o': return 4;
    case 'u': return 5;
  }
  return 0;
}
int main(void) {
  return classify('e');
}
